/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use buck2_common::result::SharedError;
use buck2_common::result::SharedResult;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::name::CellName;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter::file_type::StarlarkFileType;
use buck2_interpreter::import_paths::HasImportPaths;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::load_module::INTERPRETER_CALCULATION_IMPL;
use buck2_interpreter::path::StarlarkPath;
use dice::DiceTransaction;
use dupe::Dupe;

/// The "globals" for a path are defined by its CellName and its path type.
///
/// To compute the globals we need the Rust-level globals, the prelude, and
/// any pre-imported paths. Figuring out the names in those requires evaluating
/// Starlark code, which might fail.
pub(crate) struct CachedGlobals<'a> {
    dice: &'a DiceTransaction,
    cached: HashMap<(CellName, StarlarkFileType), SharedResult<Arc<HashSet<String>>>>,
}

impl<'a> CachedGlobals<'a> {
    pub(crate) fn new(dice: &'a DiceTransaction) -> CachedGlobals<'a> {
        Self {
            dice,
            cached: HashMap::new(),
        }
    }

    async fn load_module(&self, path: &ImportPath) -> anyhow::Result<LoadedModule> {
        self.dice.get_loaded_module_from_import_path(path).await
    }

    async fn compute_names(
        &self,
        cell: CellName,
        path: StarlarkFileType,
    ) -> anyhow::Result<HashSet<String>> {
        let mut res = HashSet::new();

        // Find the information from the globals
        let globals = INTERPRETER_CALCULATION_IMPL
            .get()?
            .global_env_for_file_type(self.dice, path)
            .await?;
        for x in globals.names() {
            res.insert(x.as_str().to_owned());
        }

        // Next grab the prelude, unless we are in the prelude cell and not a build file
        if let Some(prelude) = INTERPRETER_CALCULATION_IMPL
            .get()?
            .prelude_import(self.dice)
            .await?
        {
            if path == StarlarkFileType::Buck || prelude.cell() != cell {
                let env = self.load_module(&prelude).await?;
                for x in env.env().names() {
                    res.insert(x.as_str().to_owned());
                }
                if path == StarlarkFileType::Buck {
                    if let Some(native) = env.env().get_option("native")? {
                        let native = native.value();
                        for attr in native.dir_attr() {
                            res.insert(attr.to_owned());
                        }
                    }
                }
            }
        }

        // Now grab the pre-load things
        let import_paths = self
            .dice
            .import_paths_for_cell(BuildFileCell::new(cell))
            .await?;
        if let Some(root) = import_paths.root_import() {
            let env = self.load_module(root).await?;
            for x in env.env().names() {
                res.insert(x.as_str().to_owned());
            }
        }

        Ok(res)
    }

    pub(crate) async fn get_names(
        &mut self,
        path: &StarlarkPath<'_>,
    ) -> SharedResult<Arc<HashSet<String>>> {
        let path_type = path.file_type();
        let cell = path.cell();
        if let Some(res) = self.cached.get(&(cell, path_type)) {
            return res.dupe();
        }
        let res = match self.compute_names(cell, path_type).await {
            Ok(v) => Ok(Arc::new(v)),
            Err(e) => Err(SharedError::new(e)),
        };
        self.cached.insert((cell, path_type), res.dupe());
        res
    }
}
