// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use self::ImportDirectiveSubclass::*;

use DefModifiers;
use Module;
use Namespace::{self, TypeNS, ValueNS};
use NsDef;
use NameSearchType;
use ResolveResult;
use ResolveResult::*;
use Resolver;
use UseLexicalScopeFlag;
use {names_to_string, module_to_string};
use {resolve_error, ResolutionError};

use build_reduced_graph;

use rustc::middle::def::*;
use rustc::middle::def_id::DefId;
use rustc::middle::privacy::*;

use syntax::ast::{NodeId, Name};
use syntax::attr::AttrMetaMethods;
use syntax::codemap::Span;

use std::mem::replace;
use std::rc::Rc;


/// Contains data for specific types of import directives.
#[derive(Copy, Clone,Debug)]
pub enum ImportDirectiveSubclass {
    SingleImport(Name /* target */, Name /* source */),
    GlobImport,
}

/// Whether an import can be shadowed by another import.
#[derive(Debug,PartialEq,Clone,Copy)]
pub enum Shadowable {
    Always,
    Never,
}

/// One import directive.
#[derive(Debug)]
pub struct ImportDirective {
    pub module_path: Vec<Name>,
    pub subclass: ImportDirectiveSubclass,
    pub span: Span,
    pub id: NodeId,
    pub is_public: bool, // see note in ImportResolution about how to use this
    pub shadowable: Shadowable,
}

impl ImportDirective {
    pub fn new(module_path: Vec<Name>,
               subclass: ImportDirectiveSubclass,
               span: Span,
               id: NodeId,
               is_public: bool,
               shadowable: Shadowable)
               -> ImportDirective {
        ImportDirective {
            module_path: module_path,
            subclass: subclass,
            span: span,
            id: id,
            is_public: is_public,
            shadowable: shadowable,
        }
    }
}

/// The item that an import resolves to.
#[derive(Clone,Debug)]
pub struct Target {
    pub target_module: Rc<Module>,
    pub ns_def: NsDef,
    pub shadowable: Shadowable,
}

impl Target {
    pub fn new(target_module: Rc<Module>, ns_def: NsDef, shadowable: Shadowable) -> Target {
        Target {
            target_module: target_module,
            ns_def: ns_def,
            shadowable: shadowable,
        }
    }
}

#[derive(Debug)]
pub struct ImportResolution {
    // The number of outstanding references to this name. When this reaches
    // zero, outside modules can count on the targets being correct. Before
    // then, all bets are off; future imports could override this name.
    // Note that this is usually either 0 or 1 - shadowing is forbidden the only
    // way outstanding_references is > 1 in a legal program is if the name is
    // used in both namespaces.
    pub outstanding_references: usize,

    /// Whether this resolution came from a `use` or a `pub use`. Note that this
    /// should *not* be used whenever resolution is being performed. Privacy
    /// testing occurs during a later phase of compilation.
    pub is_public: bool,

    /// Resolution of the name in the namespace
    pub target: Option<Target>,

    /// The source node of the `use` directive
    pub id: NodeId,
}

impl ImportResolution {
    pub fn new(id: NodeId, is_public: bool) -> Self {
        ImportResolution {
            outstanding_references: 0,
            id: id,
            target: None,
            is_public: is_public,
        }
    }

    pub fn shadowable(&self) -> Shadowable {
        match self.target {
            Some(ref target) => target.shadowable,
            None => Shadowable::Always,
        }
    }
}

struct ImportResolvingError {
    span: Span,
    path: String,
    help: String,
}

struct ImportResolver<'a, 'b: 'a, 'tcx: 'b> {
    resolver: &'a mut Resolver<'b, 'tcx>,
}

impl<'a, 'b:'a, 'tcx:'b> ImportResolver<'a, 'b, 'tcx> {
    // Import resolution
    //
    // This is a fixed-point algorithm. We resolve imports until our efforts
    // are stymied by an unresolved import; then we bail out of the current
    // module and continue. We terminate successfully once no more imports
    // remain or unsuccessfully when no forward progress in resolving imports
    // is made.

    /// Resolves all imports for the crate. This method performs the fixed-
    /// point iteration.
    fn resolve_imports(&mut self) {
        let mut i = 0;
        let mut prev_unresolved_imports = 0;
        loop {
            debug!("(resolving imports) iteration {}, {} imports left",
                   i,
                   self.resolver.unresolved_imports);

            let module_root = self.resolver.graph_root.clone();
            let errors = self.resolve_imports_for_module_subtree(module_root.clone());

            if self.resolver.unresolved_imports == 0 {
                debug!("(resolving imports) success");
                break;
            }

            if self.resolver.unresolved_imports == prev_unresolved_imports {
                // resolving failed
                if errors.len() > 0 {
                    for e in errors {
                        resolve_error(self.resolver,
                                      e.span,
                                      ResolutionError::UnresolvedImport(Some((&e.path, &e.help))));
                    }
                } else {
                    // Report unresolved imports only if no hard error was already reported
                    // to avoid generating multiple errors on the same import.
                    // Imports that are still indeterminate at this point are actually blocked
                    // by errored imports, so there is no point reporting them.
                    self.resolver.report_unresolved_imports(module_root);
                }
                break;
            }

            i += 1;
            prev_unresolved_imports = self.resolver.unresolved_imports;
        }
    }

    /// Attempts to resolve imports for the given module and all of its
    /// submodules.
    fn resolve_imports_for_module_subtree(&mut self,
                                          module_: Rc<Module>)
                                          -> Vec<ImportResolvingError> {
        let mut errors = Vec::new();
        debug!("(resolving imports for module subtree) resolving {}",
               module_to_string(&*module_));
        let orig_module = replace(&mut self.resolver.current_module, module_.clone());
        errors.extend(self.resolve_imports_for_module(module_.clone()));
        self.resolver.current_module = orig_module;

        build_reduced_graph::populate_module_if_necessary(self.resolver, &module_);
        for (_, child_node) in module_.children.borrow().iter() {
            match child_node.module() {
                None => {
                    // Nothing to do.
                }
                Some(child_module) => {
                    errors.extend(self.resolve_imports_for_module_subtree(child_module));
                }
            }
        }

        for (_, child_module) in module_.anonymous_children.borrow().iter() {
            errors.extend(self.resolve_imports_for_module_subtree(child_module.clone()));
        }

        errors
    }

    /// Attempts to resolve imports for the given module only.
    fn resolve_imports_for_module(&mut self, module: Rc<Module>) -> Vec<ImportResolvingError> {
        let mut errors = Vec::new();

        if module.all_imports_resolved() {
            debug!("(resolving imports for module) all imports resolved for {}",
                   module_to_string(&*module));
            return errors;
        }

        let mut imports = module.imports.borrow_mut();
        let import_count = imports.len();
        let mut indeterminate_imports = Vec::new();
        while module.resolved_import_count.get() + indeterminate_imports.len() < import_count {
            let import_index = module.resolved_import_count.get();
            match self.resolve_import_for_module(module.clone(), &imports[import_index]) {
                ResolveResult::Failed(err) => {
                    let import_directive = &imports[import_index];
                    let (span, help) = match err {
                        Some((span, msg)) => (span, format!(". {}", msg)),
                        None => (import_directive.span, String::new()),
                    };
                    errors.push(ImportResolvingError {
                        span: span,
                        path: import_path_to_string(&import_directive.module_path,
                                                    import_directive.subclass),
                        help: help,
                    });
                }
                ResolveResult::Indeterminate => {}
                ResolveResult::Success(()) => {
                    // count success
                    module.resolved_import_count
                          .set(module.resolved_import_count.get() + 1);
                    continue;
                }
            }
            // This resolution was not successful, keep it for later
            indeterminate_imports.push(imports.swap_remove(import_index));

        }

        imports.extend(indeterminate_imports);

        errors
    }

    /// Attempts to resolve the given import. The return value indicates
    /// failure if we're certain the name does not exist, indeterminate if we
    /// don't know whether the name exists at the moment due to other
    /// currently-unresolved imports, or success if we know the name exists.
    /// If successful, the resolved bindings are written into the module.
    fn resolve_import_for_module(&mut self,
                                 module_: Rc<Module>,
                                 import_directive: &ImportDirective)
                                 -> ResolveResult<()> {
        let mut resolution_result = ResolveResult::Failed(None);
        let module_path = &import_directive.module_path;

        debug!("(resolving import for module) resolving import `{}::...` in `{}`",
               names_to_string(&module_path[..]),
               module_to_string(&*module_));

        // First, resolve the module path for the directive, if necessary.
        let container = if module_path.is_empty() {
            // Use the crate root.
            Some((self.resolver.graph_root.clone(), LastMod(AllPublic)))
        } else {
            match self.resolver.resolve_module_path(module_.clone(),
                                                    &module_path[..],
                                                    UseLexicalScopeFlag::DontUseLexicalScope,
                                                    import_directive.span,
                                                    NameSearchType::ImportSearch) {
                ResolveResult::Failed(err) => {
                    resolution_result = ResolveResult::Failed(err);
                    None
                }
                ResolveResult::Indeterminate => {
                    resolution_result = ResolveResult::Indeterminate;
                    None
                }
                ResolveResult::Success(container) => Some(container),
            }
        };

        match container {
            None => {}
            Some((containing_module, lp)) => {
                // We found the module that the target is contained
                // within. Attempt to resolve the import within it.

                match import_directive.subclass {
                    SingleImport(target, source) => {
                        resolution_result = self.resolve_single_import(&module_,
                                                                       containing_module,
                                                                       target,
                                                                       source,
                                                                       import_directive,
                                                                       lp);
                    }
                    GlobImport => {
                        resolution_result = self.resolve_glob_import(&module_,
                                                                     containing_module,
                                                                     import_directive,
                                                                     lp);
                    }
                }
            }
        }

        // Decrement the count of unresolved imports.
        match resolution_result {
            ResolveResult::Success(()) => {
                assert!(self.resolver.unresolved_imports >= 1);
                self.resolver.unresolved_imports -= 1;
            }
            _ => {
                // Nothing to do here; just return the error.
            }
        }

        // Decrement the count of unresolved globs if necessary. But only if
        // the resolution result is a success -- other cases will
        // be handled by the main loop.

        if resolution_result.success() {
            match import_directive.subclass {
                GlobImport => {
                    module_.dec_glob_count();
                    if import_directive.is_public {
                        module_.dec_pub_glob_count();
                    }
                }
                SingleImport(..) => {
                    // Ignore.
                }
            }
            if import_directive.is_public {
                module_.dec_pub_count();
            }
        }

        return resolution_result;
    }

    fn get_binding(&mut self,
                   import_resolution: &ImportResolution,
                   namespace: Namespace,
                   source: Name)
                   -> ResolveResult<(Rc<Module>, NsDef)> {
        // Import resolutions must be declared with "pub"
        // in order to be exported.
        if !import_resolution.is_public {
            return Failed(None);
        }

        match import_resolution.target.clone() {
            None => Failed(None),
            Some(Target { target_module, ns_def, shadowable: _ }) => {
                debug!("(resolving single import) found import in ns {:?}", namespace);
                let id = import_resolution.id;
                // track used imports and extern crates as well
                self.resolver.used_imports.insert((id, namespace));
                self.resolver.record_import_use(id, source);
                if let Some(DefId { krate, .. }) = target_module.def_id() {
                    self.resolver.used_crates.insert(krate);
                }
                Success((target_module, ns_def))
            }
        }
    }


    fn resolve_name(&mut self, module: &Rc<Module>, name: Name, ns: Namespace,
                    directive: &ImportDirective, pub_err: &mut bool)
                    -> ResolveResult<(Rc<Module>, NsDef)> {
        let mut result: ResolveResult<(Rc<Module>, NsDef)> = Indeterminate;

        // Search for direct children of the containing module.
        build_reduced_graph::populate_module_if_necessary(self.resolver, module);

        if let Some(ns_def) = module.get_child(name, ns) {
            debug!("(resolving single import) found {} binding",
                   match ns { ValueNS => "value", TypeNS => "type" });

            result = Success((module.clone(), ns_def.clone()));

            if !*pub_err && directive.is_public && !ns_def.is_public() {
                let msg = format!("`{}` is private, and cannot be reexported", name);
                let note_msg = if let ValueNS = ns {
                    span_err!(self.resolver.session, directive.span, E0364, "{}", &msg);
                    format!("Consider marking `{}` as `pub` in the imported module", name)
                } else {
                    span_err!(self.resolver.session, directive.span, E0365, "{}", &msg);
                    format!("Consider declaring module `{}` as a `pub mod`", name)
                };
                self.resolver.session.span_note(directive.span, &note_msg);
                *pub_err = true;
            }
        }

        result
    }

    fn resolve_single_import(&mut self,
                             module_: &Module,
                             target_module: Rc<Module>,
                             target: Name,
                             source: Name,
                             directive: &ImportDirective,
                             lp: LastPrivate)
                             -> ResolveResult<()> {
        let lp = match lp { LastMod(lp) => lp, LastImport {..} => panic!() };

        // pub_err makes sure we don't give the same error twice.
        let mut pub_err = false;

        // We need to resolve both namespaces for this to succeed.
        let (value_result, value_used_reexport) =
            self.do_resolve(&target_module, source, ValueNS, module_, directive, &mut pub_err);
        if let Indeterminate = value_result { return Indeterminate }

        let (type_result, type_used_reexport) =
            self.do_resolve(&target_module, source, TypeNS, module_, directive, &mut pub_err);
        if let Indeterminate = type_result { return Indeterminate }

        if value_result.failed() && type_result.failed() {
            let msg = format!("There is no `{}` in `{}`",
                              source,
                              module_to_string(&target_module));
            return Failed(Some((directive.span, msg)));
        }

        // We've successfully resolved the import. Write the results in.
        let value_used_public = self.check_and_write_import(module_, directive, target,
                                    ValueNS, &value_result);
        let value_used_public = value_used_reexport || value_used_public;
        self.record_import_resolution(module_, directive, target, ValueNS, value_used_public, lp);

        let type_used_public = self.check_and_write_import(module_, directive, target,
                                    TypeNS, &type_result);
        let type_used_public = type_used_reexport || type_used_public;
        self.record_import_resolution(module_, directive, target, TypeNS, type_used_public, lp);

        debug!("(resolving single import) successfully resolved import");
        return Success(());
    }

    fn do_resolve(&mut self, module: &Rc<Module>, name: Name, ns: Namespace,
                  origin_module: &Module, directive: &ImportDirective, pub_err: &mut bool)
                  -> (ResolveResult<(Rc<Module>, NsDef)>, bool) {
        let mut used_reexport = false;

        let result = self.resolve_name(module, name, ns, directive, pub_err);
        let result = result.or(|| {
            self.resolve_in_imports(module, name, ns, origin_module, &mut used_reexport)
        });
        if let Indeterminate = result { return (Indeterminate, used_reexport) }

        // If we didn't find a result in the type namespace, search the
        // external modules.
        (match result {
            Failed(_) if ns == TypeNS => {
                match module.external_module_children.borrow_mut().get(&name).cloned() {
                    None => result,
                    Some(result_module) => {
                        debug!("(resolving single import) found external module");
                        // track the module as used.
                        match result_module.def_id() {
                            Some(DefId { krate: kid, .. }) => {
                                self.resolver.used_crates.insert(kid);
                            }
                            _ => {}
                        }
                        let ns_def = NsDef::create_from_module(result_module, None);
                        Success((module.clone(), ns_def))
                    }
                }
            }
            _ => result,
        }, used_reexport)
    }


    fn resolve_in_imports(&mut self,
                          module: &Module,
                          name: Name,
                          ns: Namespace,
                          origin_module: &Module, used: &mut bool)
                          -> ResolveResult<(Rc<Module>, NsDef)> {
        // If there is an unresolved glob at this point in the
        // containing module, bail out. We don't know enough to be
        // able to resolve this import.
        if module.pub_glob_count.get() > 0 {
            debug!("(resolving single import) unresolved pub glob; bailing out");
            return Indeterminate;
        }

        // Now search the exported imports within the containing module.
        match module.import_resolutions.borrow().get(&(name, ns)) {
            // The containing module definitely doesn't have an exported import with the name
            // in question. We can therecore accurately report that the names are unbound.
            None => Failed(None),

            // The name is an import which has been fully resolved.
            // We can, therefore, just follow it.
            Some(import_resolution) if import_resolution.outstanding_references == 0 => {
                *used = import_resolution.is_public;
                self.get_binding(import_resolution, ns, name)
            },

            // If module is the same as the original module whose import we are resolving and
            // there it has an unresolved import with the same name as `source`, then the user
            // is actually trying to import an item that is declared in the same scope.
            //
            // e.g
            // use self::submodule;
            // pub mod submodule;
            //
            // In this case we continue as if we resolved the import and let
            // check_for_conflicts_between_imports_and_items handle the conflict.
            Some(_) => match (origin_module.def_id(), module.def_id()) {
                (Some(id1), Some(id2)) if id1 == id2 => Failed(None),
                _ => Indeterminate,
            },
        }
    }

    fn check_and_write_import(&mut self,
                              module: &Module,
                              directive: &ImportDirective,
                              target: Name,
                              ns: Namespace,
                              result: &ResolveResult<(Rc<Module>, NsDef)>) -> bool {
        let mut import_resolutions = module.import_resolutions.borrow_mut();
        let import_resolution = import_resolutions.get_mut(&(target, ns)).unwrap();

        let ns_name = match ns { TypeNS => "type", ValueNS => "value" };

        let used_public = match *result {
            Success((ref target_module, ref ns_def)) => {
                debug!("(resolving single import) found {:?} target: {:?}",
                       ns_name,
                       ns_def.def());
                self.check_for_conflicting_import(&import_resolution,
                                                  directive.span,
                                                  target, ns);

                self.check_that_import_is_importable(ns_def, directive.span, target);

                let target = Target::new(target_module.clone(),
                                         ns_def.clone(),
                                         directive.shadowable);
                import_resolution.target = Some(target);
                import_resolution.id = directive.id;
                import_resolution.is_public = directive.is_public;

                ns_def.is_public()
            }
            Failed(_) => false,
            Indeterminate => {
                panic!("{:?} result should be known at this point", ns_name);
            }
        };

        self.check_for_conflicts_between_imports_and_items(module,
                                                           import_resolution,
                                                           directive.span,
                                                           (target, ns));

        used_public
    }

    fn record_import_resolution(&self,
                                module: &Module,
                                directive: &ImportDirective,
                                target: Name,
                                ns: Namespace,
                                used_public: bool,
                                lp: PrivateDep) {
        let mut import_resolutions = module.import_resolutions.borrow_mut();
        let import_resolution = import_resolutions.get_mut(&(target, ns)).unwrap();

        assert!(import_resolution.outstanding_references >= 1);
        import_resolution.outstanding_references -= 1;

        let def = match import_resolution.target {
            Some(ref target) => target.ns_def.def().unwrap(),
            None => return,
        };

        let priv_dep = if used_public {
            lp
        } else {
            DependsOn(def.def_id())
        };

        let mut def_map = self.resolver.def_map.borrow_mut();
        let mut resolution = def_map.entry(directive.id).or_insert_with(|| {
            PathResolution {
                base_def: def,
                last_private: LastImport {
                    value_priv: None, value_used: Used, type_priv: None, type_used: Used,
                },
                depth: 0,
            }
        });

        if let TypeNS = ns { resolution.base_def = def; }
        match resolution.last_private {
            LastImport { ref mut value_priv, ref mut type_priv, .. } => match ns {
                ValueNS => { *value_priv = Some(priv_dep); }
                TypeNS => { *type_priv = Some(priv_dep); }
            },
            _ => panic!("Expected LastImport"),
        }
    }

    // Resolves a glob import. Note that this function cannot fail; it either
    // succeeds or bails out (as importing * from an empty module or a module
    // that exports nothing is valid). target_module is the module we are
    // actually importing, i.e., `foo` in `use foo::*`.
    fn resolve_glob_import(&mut self,
                           module_: &Module,
                           target_module: Rc<Module>,
                           import_directive: &ImportDirective,
                           lp: LastPrivate)
                           -> ResolveResult<()> {
        let id = import_directive.id;
        let is_public = import_directive.is_public;

        // This function works in a highly imperative manner; it eagerly adds
        // everything it can to the list of import resolutions of the module
        // node.
        debug!("(resolving glob import) resolving glob import {}", id);

        // We must bail out if the node has unresolved imports of any kind
        // (including globs).
        if (*target_module).pub_count.get() > 0 {
            debug!("(resolving glob import) target module has unresolved pub imports; bailing out");
            return Indeterminate;
        }

        // Add all resolved imports from the containing module.
        let import_resolutions = target_module.import_resolutions.borrow();

        if module_.import_resolutions.borrow_state() != ::std::cell::BorrowState::Unused {
            // In this case, target_module == module_
            // This means we are trying to glob import a module into itself,
            // and it is a no-go
            debug!("(resolving glob imports) target module is current module; giving up");
            return Failed(Some((import_directive.span,
                                               "Cannot glob-import a module into itself.".into())));
        }

        for (&(name, ns), target_import_resolution) in import_resolutions.iter() {
            debug!("(resolving glob import) writing module resolution {} into `{}`",
                   name,
                   module_to_string(module_));

            // Here we merge two import resolutions.
            let mut import_resolutions = module_.import_resolutions.borrow_mut();
            if let Some(dest_import_resolution) = import_resolutions.get_mut(&(name, ns)) {
                // Merge the two import resolutions at a finer-grained
                // level.
                if !target_import_resolution.is_public { continue }

                if let Some(ref target) = target_import_resolution.target {
                    self.check_for_conflicting_import(&dest_import_resolution,
                                                      import_directive.span,
                                                      name,
                                                      ns);
                    dest_import_resolution.target = Some(target.clone());
                    dest_import_resolution.is_public = is_public;
                }
                
                continue
            }

            // Simple: just copy the old import resolution.
            let mut new_import_resolution = ImportResolution::new(id, is_public);
            if !target_import_resolution.is_public { continue }
            new_import_resolution.target =
                target_import_resolution.target.clone();
            import_resolutions.insert((name, ns), new_import_resolution);
        }

        // Add all children from the containing module.
        build_reduced_graph::populate_module_if_necessary(self.resolver, &target_module);

        for (&name, ns_def) in target_module.children.borrow().iter() {
            self.merge_import_resolution(module_,
                                         target_module.clone(),
                                         import_directive,
                                         name,
                                         ns_def.clone());

        }

        // Add external module children from the containing module.
        for (&name, module) in target_module.external_module_children.borrow().iter() {
            self.merge_import_resolution(module_,
                                         target_module.clone(),
                                         import_directive,
                                         (name, TypeNS),
                                         NsDef::create_from_module(module.clone(), None));
        }

        // Record the destination of this import
        if let Some(did) = target_module.def_id() {
            self.resolver.def_map.borrow_mut().insert(id,
                                                      PathResolution {
                                                          base_def: DefMod(did),
                                                          last_private: lp,
                                                          depth: 0,
                                                      });
        }

        debug!("(resolving glob import) successfully resolved import");
        return Success(());
    }

    fn merge_import_resolution(&mut self,
                               module_: &Module,
                               containing_module: Rc<Module>,
                               import_directive: &ImportDirective,
                               (name, namespace): (Name, Namespace),
                               ns_def: NsDef) {
        let id = import_directive.id;
        let is_public = import_directive.is_public;

        let mut import_resolutions = module_.import_resolutions.borrow_mut();
        let dest_import_resolution = import_resolutions.entry((name, namespace))
                                                       .or_insert_with(|| {
                                                           ImportResolution::new(id, is_public)
                                                       });

        debug!("(resolving glob import) writing resolution `{}` in `{}` to `{}`",
               name,
               module_to_string(&*containing_module),
               module_to_string(module_));

        // Merge the child item into the import resolution.
        let modifier = DefModifiers::IMPORTABLE | DefModifiers::PUBLIC;

        if ns_def.defined_with(modifier) {
            let namespace_name = match namespace {
                TypeNS => "type",
                ValueNS => "value",
            };
            debug!("(resolving glob import) ... for {} target", namespace_name);
            if dest_import_resolution.shadowable() == Shadowable::Never {
                let msg = format!("a {} named `{}` has already been imported in this \
                                   module",
                                 namespace_name,
                                 name);
               span_err!(self.resolver.session,
                         import_directive.span,
                         E0251,
                         "{}",
                        msg);
           } else {
                let target = Target::new(containing_module.clone(),
                                         ns_def.clone(),
                                         import_directive.shadowable);
                dest_import_resolution.target = Some(target);
                dest_import_resolution.id = id;
                dest_import_resolution.is_public = is_public;
            }
        }

        self.check_for_conflicts_between_imports_and_items(module_,
                                                           dest_import_resolution,
                                                           import_directive.span,
                                                           (name, namespace));
    }

    /// Checks that imported names and items don't have the same name.
    fn check_for_conflicting_import(&mut self,
                                    import_resolution: &ImportResolution,
                                    import_span: Span,
                                    name: Name,
                                    namespace: Namespace) {
        let target = import_resolution.target.clone();
        debug!("check_for_conflicting_import: {}; target exists: {}",
               name,
               target.is_some());

        match target {
            Some(ref target) if target.shadowable != Shadowable::Always => {
                let ns_word = match namespace {
                    TypeNS => {
                        match target.ns_def.module() {
                            Some(ref module) if module.is_normal() => "module",
                            Some(ref module) if module.is_trait() => "trait",
                            _ => "type",
                        }
                    }
                    ValueNS => "value",
                };
                span_err!(self.resolver.session,
                          import_span,
                          E0252,
                          "a {} named `{}` has already been imported in this module",
                          ns_word,
                          name);
                let use_id = import_resolution.id;
                let item = self.resolver.ast_map.expect_item(use_id);
                // item is syntax::ast::Item;
                span_note!(self.resolver.session,
                           item.span,
                           "previous import of `{}` here",
                           name);
            }
            Some(_) | None => {}
        }
    }

    /// Checks that an import is actually importable
    fn check_that_import_is_importable(&mut self,
                                       ns_def: &NsDef,
                                       import_span: Span,
                                       name: Name) {
        if !ns_def.defined_with(DefModifiers::IMPORTABLE) {
            let msg = format!("`{}` is not directly importable", name);
            span_err!(self.resolver.session, import_span, E0253, "{}", &msg[..]);
        }
    }

    /// Checks that imported names and items don't have the same name.
    fn check_for_conflicts_between_imports_and_items(&mut self,
                                                     module: &Module,
                                                     import_resolution: &ImportResolution,
                                                     import_span: Span,
                                                     (name, ns): (Name, Namespace)) {
        // First, check for conflicts between imports and `extern crate`s.
        if let TypeNS = ns {
            if module.external_module_children.borrow().contains_key(&name) {
                match import_resolution.target {
                    Some(ref target) if target.shadowable != Shadowable::Always => {
                        let msg = format!("import `{0}` conflicts with imported crate \
                                           in this module (maybe you meant `use {0}::*`?)",
                                          name);
                        span_err!(self.resolver.session, import_span, E0254, "{}", &msg[..]);
                    }
                    Some(_) | None => {}
                }
            }
        }

        // Check for item conflicts.
        let ns_def = match module.get_child(name, ns) {
            None => {
                // There can't be any conflicts.
                return;
            }
            Some(ns_def) => ns_def,
        };

        if let ValueNS = ns {
            match import_resolution.target {
                Some(ref target) if target.shadowable != Shadowable::Always => {
                    span_err!(self.resolver.session,
                              import_span,
                              E0255,
                              "import `{}` conflicts with value in this module",
                              name);
                    if let Some(span) = ns_def.span {
                        self.resolver.session.span_note(span, "conflicting value here");
                    }
                }
                Some(_) | None => {}
            }
        } else {
            match import_resolution.target {
                Some(ref target) if target.shadowable != Shadowable::Always => {
                    let (what, note) = match ns_def.module() {
                        Some(ref module) if module.is_normal() =>
                            ("existing submodule", "note conflicting module here"),
                        Some(ref module) if module.is_trait() =>
                            ("trait in this module", "note conflicting trait here"),
                        _ => ("type in this module", "note conflicting type here"),
                    };
                    span_err!(self.resolver.session,
                              import_span,
                              E0256,
                              "import `{}` conflicts with {}",
                              name,
                              what);
                    if let Some(span) = ns_def.span {
                        self.resolver.session.span_note(span, note);
                    }
                }
                Some(_) | None => {}
            }
        }
    }
}

fn import_path_to_string(names: &[Name], subclass: ImportDirectiveSubclass) -> String {
    if names.is_empty() {
        import_directive_subclass_to_string(subclass)
    } else {
        (format!("{}::{}",
                 names_to_string(names),
                 import_directive_subclass_to_string(subclass)))
            .to_string()
    }
}

fn import_directive_subclass_to_string(subclass: ImportDirectiveSubclass) -> String {
    match subclass {
        SingleImport(_, source) => source.to_string(),
        GlobImport => "*".to_string(),
    }
}

pub fn resolve_imports(resolver: &mut Resolver) {
    let mut import_resolver = ImportResolver { resolver: resolver };
    import_resolver.resolve_imports();
}
