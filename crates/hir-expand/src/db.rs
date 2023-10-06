//! Defines database & queries for macro expansion.

use ::tt::SyntaxContext;
use base_db::{
    salsa,
    span::{SpanAnchor, SyntaxContextId, ROOT_ERASED_FILE_AST_ID},
    CrateId, Edition, SourceDatabase,
};
use either::Either;
use limit::Limit;
use mbe::{map_from_syntax_node, syntax_node_to_token_tree, ValueResult};
use syntax::{
    ast::{self, HasAttrs, HasDocComments},
    AstNode, Parse, SyntaxError, SyntaxNode, SyntaxToken, TextSize, T,
};
use triomphe::Arc;

use crate::{
    ast_id_map::AstIdMap,
    builtin_attr_macro::pseudo_derive_attr_expansion,
    builtin_fn_macro::EagerExpander,
    hygiene::{self, HygieneFrame, SyntaxContextData},
    tt, AstId, BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerCallInfo,
    ExpandError, ExpandResult, ExpandTo, HirFileId, HirFileIdRepr, MacroCallId, MacroCallKind,
    MacroCallLoc, MacroDefId, MacroDefKind, MacroFile, ProcMacroExpander, SpanMap,
};

/// Total limit on the number of tokens produced by any macro invocation.
///
/// If an invocation produces more tokens than this limit, it will not be stored in the database and
/// an error will be emitted.
///
/// Actual max for `analysis-stats .` at some point: 30672.
static TOKEN_LIMIT: Limit = Limit::new(1_048_576);

#[derive(Debug, Clone, Eq, PartialEq)]
/// Old-style `macro_rules` or the new macros 2.0
pub struct DeclarativeMacroExpander {
    pub mac: mbe::DeclarativeMacro<base_db::span::SpanData>,
}

impl DeclarativeMacroExpander {
    pub fn expand(&self, tt: tt::Subtree) -> ExpandResult<tt::Subtree> {
        match self.mac.err() {
            Some(e) => ExpandResult::new(
                tt::Subtree::empty(),
                ExpandError::other(format!("invalid macro definition: {e}")),
            ),
            None => self.mac.expand(&tt).map_err(Into::into),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TokenExpander {
    /// Old-style `macro_rules` or the new macros 2.0
    DeclarativeMacro(Arc<DeclarativeMacroExpander>),
    /// Stuff like `line!` and `file!`.
    BuiltIn(BuiltinFnLikeExpander),
    /// Built-in eagerly expanded fn-like macros (`include!`, `concat!`, etc.)
    BuiltInEager(EagerExpander),
    /// `global_allocator` and such.
    BuiltInAttr(BuiltinAttrExpander),
    /// `derive(Copy)` and such.
    BuiltInDerive(BuiltinDeriveExpander),
    /// The thing we love the most here in rust-analyzer -- procedural macros.
    ProcMacro(ProcMacroExpander),
}

#[salsa::query_group(ExpandDatabaseStorage)]
pub trait ExpandDatabase: SourceDatabase {
    fn ast_id_map(&self, file_id: HirFileId) -> Arc<AstIdMap>;

    /// Main public API -- parses a hir file, not caring whether it's a real
    /// file or a macro expansion.
    #[salsa::transparent]
    fn parse_or_expand(&self, file_id: HirFileId) -> SyntaxNode;
    #[salsa::transparent]
    fn parse_or_expand_with_err(&self, file_id: HirFileId) -> ExpandResult<Parse<SyntaxNode>>;
    /// Implementation for the macro case.
    // This query is LRU cached
    fn parse_macro_expansion(
        &self,
        macro_file: MacroFile,
    ) -> ExpandResult<(Parse<SyntaxNode>, Arc<SpanMap>)>;

    /// Macro ids. That's probably the tricksiest bit in rust-analyzer, and the
    /// reason why we use salsa at all.
    ///
    /// We encode macro definitions into ids of macro calls, this what allows us
    /// to be incremental.
    #[salsa::interned]
    fn intern_macro_call(&self, macro_call: MacroCallLoc) -> MacroCallId;
    #[salsa::interned]
    fn intern_syntax_context(&self, ctx: SyntaxContextData) -> SyntaxContextId;
    #[salsa::transparent]
    #[salsa::invoke(hygiene::apply_mark)]
    fn apply_mark(
        &self,
        ctxt: SyntaxContextData,
        file_id: HirFileId,
        transparency: hygiene::Transparency,
    ) -> SyntaxContextId;

    /// Lowers syntactic macro call to a token tree representation. That's a firewall
    /// query, only typing in the macro call itself changes the returned
    /// subtree.
    fn macro_arg(
        &self,
        id: MacroCallId,
    ) -> ValueResult<Option<Arc<tt::Subtree>>, Arc<Box<[SyntaxError]>>>;
    /// Fetches the expander for this macro.
    #[salsa::transparent]
    fn macro_expander(&self, id: MacroDefId) -> TokenExpander;
    /// Fetches (and compiles) the expander of this decl macro.
    fn decl_macro_expander(
        &self,
        def_crate: CrateId,
        id: AstId<ast::Macro>,
    ) -> Arc<DeclarativeMacroExpander>;

    /// Expand macro call to a token tree.
    // This query is LRU cached
    fn macro_expand(&self, macro_call: MacroCallId) -> ExpandResult<Arc<tt::Subtree>>;
    #[salsa::invoke(crate::builtin_fn_macro::include_arg_to_tt)]
    fn include_expand(
        &self,
        arg_id: MacroCallId,
    ) -> Result<(triomphe::Arc<tt::Subtree>, base_db::FileId), ExpandError>;
    /// Special case of the previous query for procedural macros. We can't LRU
    /// proc macros, since they are not deterministic in general, and
    /// non-determinism breaks salsa in a very, very, very bad way.
    /// @edwin0cheng heroically debugged this once! See #4315 for details
    fn expand_proc_macro(&self, call: MacroCallId) -> ExpandResult<Arc<tt::Subtree>>;
    /// Firewall query that returns the errors from the `parse_macro_expansion` query.
    fn parse_macro_expansion_error(
        &self,
        macro_call: MacroCallId,
    ) -> ExpandResult<Box<[SyntaxError]>>;

    fn hygiene_frame(&self, file_id: HirFileId) -> Arc<HygieneFrame>;
}

/// This expands the given macro call, but with different arguments. This is
/// used for completion, where we want to see what 'would happen' if we insert a
/// token. The `token_to_map` mapped down into the expansion, with the mapped
/// token returned.
pub fn expand_speculative(
    db: &dyn ExpandDatabase,
    actual_macro_call: MacroCallId,
    speculative_args: &SyntaxNode,
    token_to_map: SyntaxToken,
) -> Option<(SyntaxNode, SyntaxToken)> {
    let loc = db.lookup_intern_macro_call(actual_macro_call);
    let file_id = loc.kind.file_id();

    // Build the subtree and token mapping for the speculative args
    let _censor = censor_for_macro_input(&loc, speculative_args);
    let mut tt = mbe::syntax_node_to_token_tree(
        speculative_args,
        // we don't leak these spans into any query so its fine to make them absolute
        SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
        TextSize::new(0),
        &Default::default(),
    );

    let attr_arg = match loc.kind {
        MacroCallKind::Attr { invoc_attr_index, .. } => {
            let attr = if loc.def.is_attribute_derive() {
                // for pseudo-derive expansion we actually pass the attribute itself only
                ast::Attr::cast(speculative_args.clone())
            } else {
                // Attributes may have an input token tree, build the subtree and map for this as well
                // then try finding a token id for our token if it is inside this input subtree.
                let item = ast::Item::cast(speculative_args.clone())?;
                item.doc_comments_and_attrs()
                    .nth(invoc_attr_index.ast_index())
                    .and_then(Either::left)
            }?;
            match attr.token_tree() {
                Some(token_tree) => {
                    let mut tree = syntax_node_to_token_tree(
                        token_tree.syntax(),
                        SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
                        TextSize::new(0),
                        &Default::default(),
                    );
                    tree.delimiter = tt::Delimiter::UNSPECIFIED;

                    Some(tree)
                }
                _ => None,
            }
        }
        _ => None,
    };

    // Do the actual expansion, we need to directly expand the proc macro due to the attribute args
    // Otherwise the expand query will fetch the non speculative attribute args and pass those instead.
    let speculative_expansion = match loc.def.kind {
        MacroDefKind::ProcMacro(expander, ..) => {
            tt.delimiter = tt::Delimiter::UNSPECIFIED;
            expander.expand(db, loc.def.krate, loc.krate, &tt, attr_arg.as_ref())
        }
        MacroDefKind::BuiltInAttr(BuiltinAttrExpander::Derive, _) => {
            pseudo_derive_attr_expansion(&tt, attr_arg.as_ref()?)
        }
        MacroDefKind::BuiltInDerive(expander, ..) => {
            // this cast is a bit sus, can we avoid losing the typedness here?
            let adt = ast::Adt::cast(speculative_args.clone()).unwrap();
            expander.expand(
                db,
                actual_macro_call,
                &adt,
                &map_from_syntax_node(
                    speculative_args,
                    // we don't leak these spans into any query so its fine to make them absolute
                    SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
                    TextSize::new(0),
                ),
            )
        }
        MacroDefKind::Declarative(it) => db.decl_macro_expander(loc.krate, it).expand(tt),
        MacroDefKind::BuiltIn(it, _) => it.expand(db, actual_macro_call, &tt).map_err(Into::into),
        MacroDefKind::BuiltInEager(it, _) => {
            it.expand(db, actual_macro_call, &tt).map_err(Into::into)
        }
        MacroDefKind::BuiltInAttr(it, _) => it.expand(db, actual_macro_call, &tt),
    };

    let expand_to = macro_expand_to(db, actual_macro_call);
    let (node, rev_tmap) = token_tree_to_syntax_node(db, &speculative_expansion.value, expand_to);

    let syntax_node = node.syntax_node();
    let token = rev_tmap
        .ranges_with_span(tt::SpanData {
            range: token_to_map.text_range(),
            anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContextId::DUMMY,
        })
        .filter_map(|range| syntax_node.covering_element(range).into_token())
        .min_by_key(|t| {
            // prefer tokens of the same kind and text
            // Note the inversion of the score here, as we want to prefer the first token in case
            // of all tokens having the same score
            (t.kind() != token_to_map.kind()) as u8 + (t.text() != token_to_map.text()) as u8
        })?;
    Some((node.syntax_node(), token))
}

fn ast_id_map(db: &dyn ExpandDatabase, file_id: HirFileId) -> Arc<AstIdMap> {
    Arc::new(AstIdMap::from_source(&db.parse_or_expand(file_id)))
}

fn parse_or_expand(db: &dyn ExpandDatabase, file_id: HirFileId) -> SyntaxNode {
    match file_id.repr() {
        HirFileIdRepr::FileId(file_id) => db.parse(file_id).syntax_node(),
        HirFileIdRepr::MacroFile(macro_file) => {
            db.parse_macro_expansion(macro_file).value.0.syntax_node()
        }
    }
}

fn parse_or_expand_with_err(
    db: &dyn ExpandDatabase,
    file_id: HirFileId,
) -> ExpandResult<Parse<SyntaxNode>> {
    match file_id.repr() {
        HirFileIdRepr::FileId(file_id) => ExpandResult::ok(db.parse(file_id).to_syntax()),
        HirFileIdRepr::MacroFile(macro_file) => {
            db.parse_macro_expansion(macro_file).map(|(it, _)| it)
        }
    }
}

fn parse_macro_expansion(
    db: &dyn ExpandDatabase,
    macro_file: MacroFile,
) -> ExpandResult<(Parse<SyntaxNode>, Arc<SpanMap>)> {
    let _p = profile::span("parse_macro_expansion");
    let mbe::ValueResult { value: tt, err } = db.macro_expand(macro_file.macro_call_id);

    let expand_to = macro_expand_to(db, macro_file.macro_call_id);

    tracing::debug!("expanded = {}", tt.as_debug_string());
    tracing::debug!("kind = {:?}", expand_to);

    let (parse, rev_token_map) = token_tree_to_syntax_node(db, &tt, expand_to);

    ExpandResult { value: (parse, Arc::new(rev_token_map)), err }
}

fn parse_macro_expansion_error(
    db: &dyn ExpandDatabase,
    macro_call_id: MacroCallId,
) -> ExpandResult<Box<[SyntaxError]>> {
    db.parse_macro_expansion(MacroFile { macro_call_id })
        .map(|it| it.0.errors().to_vec().into_boxed_slice())
}

fn macro_arg(
    db: &dyn ExpandDatabase,
    id: MacroCallId,
) -> ValueResult<Option<Arc<tt::Subtree>>, Arc<Box<[SyntaxError]>>> {
    let mismatched_delimiters = |arg: &SyntaxNode| {
        let first = arg.first_child_or_token().map_or(T![.], |it| it.kind());
        let last = arg.last_child_or_token().map_or(T![.], |it| it.kind());
        let well_formed_tt =
            matches!((first, last), (T!['('], T![')']) | (T!['['], T![']']) | (T!['{'], T!['}']));
        if !well_formed_tt {
            // Don't expand malformed (unbalanced) macro invocations. This is
            // less than ideal, but trying to expand unbalanced  macro calls
            // sometimes produces pathological, deeply nested code which breaks
            // all kinds of things.
            //
            // Some day, we'll have explicit recursion counters for all
            // recursive things, at which point this code might be removed.
            cov_mark::hit!(issue9358_bad_macro_stack_overflow);
            Some(Arc::new(Box::new([SyntaxError::new(
                "unbalanced token tree".to_owned(),
                arg.text_range(),
            )]) as Box<[_]>))
        } else {
            None
        }
    };
    let loc = db.lookup_intern_macro_call(id);
    if let Some(EagerCallInfo { arg, .. }) = matches!(loc.def.kind, MacroDefKind::BuiltInEager(..))
        .then(|| loc.eager.as_deref())
        .flatten()
    {
        ValueResult::ok(Some(Arc::new(arg.0.clone())))
    } else {
        let (parse, map) = match loc.kind.file_id().repr() {
            HirFileIdRepr::FileId(file_id) => {
                (db.parse(file_id).to_syntax(), Arc::new(Default::default()))
            }
            HirFileIdRepr::MacroFile(macro_file) => {
                let (parse, map) = db.parse_macro_expansion(macro_file).value;
                (parse, map)
            }
        };
        let root = parse.syntax_node();

        let (syntax, offset, ast_id) = match loc.kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                let node = &ast_id.to_ptr(db).to_node(&root);
                let offset = node.syntax().text_range().start();
                match node.token_tree().map(|it| it.syntax().clone()) {
                    Some(tt) => {
                        if let Some(e) = mismatched_delimiters(&tt) {
                            return ValueResult::only_err(e);
                        }
                        (tt, offset, ast_id.value.erase())
                    }
                    None => {
                        return ValueResult::only_err(Arc::new(Box::new([
                            SyntaxError::new_at_offset("missing token tree".to_owned(), offset),
                        ])));
                    }
                }
            }
            MacroCallKind::Derive { ast_id, .. } => {
                let syntax_node = ast_id.to_ptr(db).to_node(&root).syntax().clone();
                let offset = syntax_node.text_range().start();
                (syntax_node, offset, ast_id.value.erase())
            }
            MacroCallKind::Attr { ast_id, .. } => {
                let syntax_node = ast_id.to_ptr(db).to_node(&root).syntax().clone();
                let offset = syntax_node.text_range().start();
                (syntax_node, offset, ast_id.value.erase())
            }
        };
        let censor = censor_for_macro_input(&loc, &syntax);
        // let mut fixups = fixup::fixup_syntax(&node);
        // fixups.replace.extend(censor.into_iter().map(|node| (node.into(), Vec::new())));
        // let (mut tt, tmap, _) = mbe::syntax_node_to_token_tree_with_modifications(
        //     &node,
        //     fixups.token_map,
        //     fixups.next_id,
        //     fixups.replace,
        //     fixups.append,
        // );
        let mut tt = mbe::syntax_node_to_token_tree_censored(
            &syntax,
            SpanAnchor { file_id: loc.kind.file_id(), ast_id },
            offset,
            &map,
            censor,
        );

        if loc.def.is_proc_macro() {
            // proc macros expect their inputs without parentheses, MBEs expect it with them included
            tt.delimiter = tt::Delimiter::UNSPECIFIED;
        }

        if matches!(loc.def.kind, MacroDefKind::BuiltInEager(..)) {
            match parse.errors() {
                [] => ValueResult::ok(Some(Arc::new(tt))),
                errors => ValueResult::new(
                    Some(Arc::new(tt)),
                    // Box::<[_]>::from(res.errors()), not stable yet
                    Arc::new(errors.to_vec().into_boxed_slice()),
                ),
            }
        } else {
            ValueResult::ok(Some(Arc::new(tt)))
        }
    }
}

// FIXME: Censoring info should be calculated by the caller! Namely by name resolution
/// Certain macro calls expect some nodes in the input to be preprocessed away, namely:
/// - derives expect all `#[derive(..)]` invocations up to the currently invoked one to be stripped
/// - attributes expect the invoking attribute to be stripped
fn censor_for_macro_input(loc: &MacroCallLoc, node: &SyntaxNode) -> Vec<SyntaxNode> {
    // FIXME: handle `cfg_attr`
    (|| {
        let censor = match loc.kind {
            MacroCallKind::FnLike { .. } => return None,
            MacroCallKind::Derive { derive_attr_index, .. } => {
                cov_mark::hit!(derive_censoring);
                ast::Item::cast(node.clone())?
                    .attrs()
                    .take(derive_attr_index.ast_index() + 1)
                    // FIXME, this resolution should not be done syntactically
                    // derive is a proper macro now, no longer builtin
                    // But we do not have resolution at this stage, this means
                    // we need to know about all macro calls for the given ast item here
                    // so we require some kind of mapping...
                    .filter(|attr| attr.simple_name().as_deref() == Some("derive"))
                    .map(|it| it.syntax().clone())
                    .collect()
            }
            MacroCallKind::Attr { .. } if loc.def.is_attribute_derive() => return None,
            MacroCallKind::Attr { invoc_attr_index, .. } => {
                cov_mark::hit!(attribute_macro_attr_censoring);
                ast::Item::cast(node.clone())?
                    .doc_comments_and_attrs()
                    .nth(invoc_attr_index.ast_index())
                    .and_then(Either::left)
                    .map(|attr| attr.syntax().clone())
                    .into_iter()
                    .collect()
            }
        };
        Some(censor)
    })()
    .unwrap_or_default()
}

fn decl_macro_expander(
    db: &dyn ExpandDatabase,
    def_crate: CrateId,
    id: AstId<ast::Macro>,
) -> Arc<DeclarativeMacroExpander> {
    let is_2021 = db.crate_graph()[def_crate].edition >= Edition::Edition2021;
    let (root, map) = match id.file_id.repr() {
        HirFileIdRepr::FileId(file_id) => {
            (db.parse(file_id).syntax_node(), Arc::new(Default::default()))
        }
        HirFileIdRepr::MacroFile(macro_file) => {
            let (parse, map) = db.parse_macro_expansion(macro_file).value;
            (parse.syntax_node(), map)
        }
    };
    let mac = match id.to_ptr(db).to_node(&root) {
        ast::Macro::MacroRules(macro_rules) => match macro_rules.token_tree() {
            Some(arg) => {
                let tt = mbe::syntax_node_to_token_tree(
                    arg.syntax(),
                    SpanAnchor { file_id: id.file_id, ast_id: id.value.erase() },
                    macro_rules.syntax().text_range().start(),
                    &map,
                );
                let mac = mbe::DeclarativeMacro::parse_macro_rules(&tt, is_2021);
                mac
            }
            None => mbe::DeclarativeMacro::from_err(
                mbe::ParseError::Expected("expected a token tree".into()),
                is_2021,
            ),
        },
        ast::Macro::MacroDef(macro_def) => match macro_def.body() {
            Some(arg) => {
                let tt = mbe::syntax_node_to_token_tree(
                    arg.syntax(),
                    SpanAnchor { file_id: id.file_id, ast_id: id.value.erase() },
                    macro_def.syntax().text_range().start(),
                    &map,
                );
                let mac = mbe::DeclarativeMacro::parse_macro2(&tt, is_2021);
                mac
            }
            None => mbe::DeclarativeMacro::from_err(
                mbe::ParseError::Expected("expected a token tree".into()),
                is_2021,
            ),
        },
    };
    Arc::new(DeclarativeMacroExpander { mac })
}

fn macro_expander(db: &dyn ExpandDatabase, id: MacroDefId) -> TokenExpander {
    match id.kind {
        MacroDefKind::Declarative(ast_id) => {
            TokenExpander::DeclarativeMacro(db.decl_macro_expander(id.krate, ast_id))
        }
        MacroDefKind::BuiltIn(expander, _) => TokenExpander::BuiltIn(expander),
        MacroDefKind::BuiltInAttr(expander, _) => TokenExpander::BuiltInAttr(expander),
        MacroDefKind::BuiltInDerive(expander, _) => TokenExpander::BuiltInDerive(expander),
        MacroDefKind::BuiltInEager(expander, ..) => TokenExpander::BuiltInEager(expander),
        MacroDefKind::ProcMacro(expander, ..) => TokenExpander::ProcMacro(expander),
    }
}

fn macro_expand(db: &dyn ExpandDatabase, id: MacroCallId) -> ExpandResult<Arc<tt::Subtree>> {
    let _p = profile::span("macro_expand");
    let loc = db.lookup_intern_macro_call(id);

    let ExpandResult { value: tt, mut err } = match loc.def.kind {
        MacroDefKind::ProcMacro(..) => return db.expand_proc_macro(id),
        MacroDefKind::BuiltInDerive(expander, ..) => {
            // FIXME: add firewall query for this?
            let hir_file_id = loc.kind.file_id();
            let (root, map) = match hir_file_id.repr() {
                HirFileIdRepr::FileId(file_id) => (db.parse(file_id).syntax_node(), None),
                HirFileIdRepr::MacroFile(macro_file) => {
                    let (parse, map) = db.parse_macro_expansion(macro_file).value;
                    (parse.syntax_node(), Some(map))
                }
            };
            let MacroCallKind::Derive { ast_id, .. } = loc.kind else { unreachable!() };
            let node = ast_id.to_ptr(db).to_node(&root);

            // FIXME: we might need to remove the spans from the input to the derive macro here
            let _censor = censor_for_macro_input(&loc, node.syntax());
            let _t;
            expander.expand(
                db,
                id,
                &node,
                match &map {
                    Some(map) => map,
                    None => {
                        _t = map_from_syntax_node(
                            node.syntax(),
                            SpanAnchor { file_id: hir_file_id, ast_id: ast_id.value.erase() },
                            node.syntax().text_range().start(),
                        );
                        &_t
                    }
                },
            )
        }
        _ => {
            let ValueResult { value, err } = db.macro_arg(id);
            let Some(macro_arg) = value else {
                return ExpandResult {
                    value: Arc::new(tt::Subtree {
                        delimiter: tt::Delimiter::UNSPECIFIED,
                        token_trees: Vec::new(),
                    }),
                    // FIXME: We should make sure to enforce an invariant that invalid macro
                    // calls do not reach this call path!
                    err: Some(ExpandError::other("invalid token tree")),
                };
            };

            let arg = &*macro_arg;
            match loc.def.kind {
                MacroDefKind::Declarative(id) => {
                    db.decl_macro_expander(loc.def.krate, id).expand(arg.clone())
                }
                MacroDefKind::BuiltIn(it, _) => it.expand(db, id, &arg).map_err(Into::into),
                // This might look a bit odd, but we do not expand the inputs to eager macros here.
                // Eager macros inputs are expanded, well, eagerly when we collect the macro calls.
                // That kind of expansion uses the ast id map of an eager macros input though which goes through
                // the HirFileId machinery. As eager macro inputs are assigned a macro file id that query
                // will end up going through here again, whereas we want to just want to inspect the raw input.
                // As such we just return the input subtree here.
                MacroDefKind::BuiltInEager(..) if loc.eager.is_none() => {
                    return ExpandResult {
                        value: Arc::new(arg.clone()),
                        err: err.map(|err| {
                            let mut buf = String::new();
                            for err in &**err {
                                use std::fmt::Write;
                                _ = write!(buf, "{}, ", err);
                            }
                            buf.pop();
                            buf.pop();
                            ExpandError::other(buf)
                        }),
                    };
                }
                MacroDefKind::BuiltInEager(it, _) => it.expand(db, id, &arg).map_err(Into::into),
                MacroDefKind::BuiltInAttr(it, _) => it.expand(db, id, &arg),
                _ => unreachable!(),
            }
        }
    };

    if let Some(EagerCallInfo { error, .. }) = loc.eager.as_deref() {
        // FIXME: We should report both errors!
        err = error.clone().or(err);
    }

    // Skip checking token tree limit for include! macro call
    if !loc.def.is_include() {
        // Set a hard limit for the expanded tt
        if let Err(value) = check_tt_count(&tt) {
            return value;
        }
    }

    ExpandResult { value: Arc::new(tt), err }
}

fn expand_proc_macro(db: &dyn ExpandDatabase, id: MacroCallId) -> ExpandResult<Arc<tt::Subtree>> {
    // FIXME: Syntax fix ups
    let loc = db.lookup_intern_macro_call(id);
    let Some(macro_arg) = db.macro_arg(id).value else {
        return ExpandResult {
            value: Arc::new(tt::Subtree {
                delimiter: tt::Delimiter::UNSPECIFIED,
                token_trees: Vec::new(),
            }),
            // FIXME: We should make sure to enforce an invariant that invalid macro
            // calls do not reach this call path!
            err: Some(ExpandError::other("invalid token tree")),
        };
    };

    let expander = match loc.def.kind {
        MacroDefKind::ProcMacro(expander, ..) => expander,
        _ => unreachable!(),
    };

    let attr_arg = match &loc.kind {
        MacroCallKind::Attr { attr_args, .. } => Some(&**attr_args),
        _ => None,
    };

    let ExpandResult { value: tt, err } =
        expander.expand(db, loc.def.krate, loc.krate, &macro_arg, attr_arg);

    // Set a hard limit for the expanded tt
    if let Err(value) = check_tt_count(&tt) {
        return value;
    }

    ExpandResult { value: Arc::new(tt), err }
}

fn hygiene_frame(db: &dyn ExpandDatabase, file_id: HirFileId) -> Arc<HygieneFrame> {
    Arc::new(HygieneFrame::new(db, file_id))
}

fn macro_expand_to(db: &dyn ExpandDatabase, id: MacroCallId) -> ExpandTo {
    db.lookup_intern_macro_call(id).expand_to()
}

fn token_tree_to_syntax_node(
    db: &dyn ExpandDatabase,
    tt: &tt::Subtree,
    expand_to: ExpandTo,
) -> (Parse<SyntaxNode>, SpanMap) {
    let entry_point = match expand_to {
        ExpandTo::Statements => mbe::TopEntryPoint::MacroStmts,
        ExpandTo::Items => mbe::TopEntryPoint::MacroItems,
        ExpandTo::Pattern => mbe::TopEntryPoint::Pattern,
        ExpandTo::Type => mbe::TopEntryPoint::Type,
        ExpandTo::Expr => mbe::TopEntryPoint::Expr,
    };
    let mut tm = mbe::token_tree_to_syntax_node(tt, entry_point);
    // now what the hell is going on here
    tm.1.span_map.sort_by(|(_, a), (_, b)| {
        a.anchor.file_id.cmp(&b.anchor.file_id).then_with(|| {
            let map = db.ast_id_map(a.anchor.file_id);
            map.get_raw(a.anchor.ast_id)
                .text_range()
                .start()
                .cmp(&map.get_raw(b.anchor.ast_id).text_range().start())
        })
    });
    tm
}

fn check_tt_count(tt: &tt::Subtree) -> Result<(), ExpandResult<Arc<tt::Subtree>>> {
    let count = tt.count();
    if TOKEN_LIMIT.check(count).is_err() {
        Err(ExpandResult {
            value: Arc::new(tt::Subtree {
                delimiter: tt::Delimiter::UNSPECIFIED,
                token_trees: vec![],
            }),
            err: Some(ExpandError::other(format!(
                "macro invocation exceeds token limit: produced {} tokens, limit is {}",
                count,
                TOKEN_LIMIT.inner(),
            ))),
        })
    } else {
        Ok(())
    }
}
