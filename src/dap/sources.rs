//! Modules, source files, and the lines breakpoints can use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use microscope::{BreakpointSpec, LineNumber, LoadedModuleRecord, ModuleId, ModuleImage};
use serde_json::{Value, json};

use super::protocol::{
    BreakpointLocationsArguments, ErrorBody, GotoTargetsArguments, ModulesArguments,
};

use super::session::{Closed, Session, error, parse};

impl Session {
    pub(super) async fn modules(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ModulesArguments>(arguments, "modules arguments")?;
        let handle = self.target_handle()?;

        let records = match handle.loaded_modules().await {
            Ok(snapshot) => snapshot.modules.to_vec(),
            Err(microscope::Error::NotRunning) => Vec::new(),
            Err(other) => return Err(error(other)),
        };

        let start = usize::try_from(arguments.start_module.unwrap_or(0)).unwrap_or(0);
        let count = arguments
            .module_count
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count != 0)
            .unwrap_or(usize::MAX);
        let mut modules = Vec::new();

        for record in records.iter().skip(start).take(count) {
            let image = self.image(record.module.id).await;
            modules.push(module_json(record, image.as_deref()));
        }

        Ok(json!({"modules": modules, "totalModules": records.len()}))
    }

    /// Brings the client's modules up to date: announces the loaded ones it
    /// has not heard about, such as the program itself, which no load event
    /// reports, and removes those unloaded while events were missed.
    pub(super) async fn announce_modules(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.target_handle().ok() else {
            return Ok(());
        };

        let Ok(snapshot) = handle.loaded_modules().await else {
            return Ok(());
        };

        let loaded = snapshot
            .modules
            .iter()
            .map(|record| record.module.id)
            .collect::<BTreeSet<_>>();
        let unloaded = self
            .modules
            .keys()
            .filter(|id| !loaded.contains(id))
            .copied()
            .collect::<Vec<_>>();
        for id in unloaded {
            self.forget_module(id).await?;
        }

        for record in snapshot.modules.iter() {
            self.announce_module(record).await?;
        }

        Ok(())
    }

    pub(super) async fn announce_module(
        &mut self,
        record: &LoadedModuleRecord,
    ) -> Result<(), Closed> {
        if self.modules.contains_key(&record.module.id) {
            return Ok(());
        }

        self.modules.insert(record.module.id, record.clone());
        let image = self.image(record.module.id).await;
        self.client
            .event(
                "module",
                json!({"reason": "new", "module": module_json(record, image.as_deref())}),
            )
            .await?;

        // What could not be used of a library's debug information, and what
        // kept its own views out, once, when it loads; the program's are
        // said when the session starts.
        if let (Some(image), Ok(handle)) = (&image, self.target_handle())
            && image.id() != handle.module_image().id()
        {
            for warning in crate::present::debug_information::warnings(image, false) {
                self.client.important(warning).await?;
            }

            for error in image.view_errors() {
                self.client.important(format!("views: {error}")).await?;
            }
        }

        self.add_loaded_sources(record.module.id, image.as_deref())
            .await
    }

    /// Lists the source files of every loaded module. From then on,
    /// `loadedSource` events keep the client's list current as modules
    /// load and unload; a client that never asks is spared an event for
    /// every file of every module.
    pub(super) async fn loaded_sources(&mut self) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        let mut sources = BTreeMap::<PathBuf, BTreeSet<ModuleId>>::new();

        // Before the program runs, its own image is what is loaded.
        if self.modules.is_empty() {
            for file in handle.module_image().source_files() {
                sources.entry(self.local_path(&file.path)).or_default();
            }
        }

        let records = self.modules.values().cloned().collect::<Vec<_>>();
        for record in records {
            if let Some(image) = self.image(record.module.id).await {
                for file in image.source_files() {
                    sources
                        .entry(self.local_path(&file.path))
                        .or_default()
                        .insert(record.module.id);
                }
            }
        }

        let body =
            json!({"sources": sources.keys().map(|path| source_json(path)).collect::<Vec<_>>()});
        self.loaded_sources = Some(sources);
        Ok(body)
    }

    /// Tells a client keeping a list of loaded sources about a module's
    /// files it did not list yet.
    async fn add_loaded_sources(
        &mut self,
        module: ModuleId,
        image: Option<&ModuleImage>,
    ) -> Result<(), Closed> {
        let (Some(image), true) = (image, self.loaded_sources.is_some()) else {
            return Ok(());
        };

        let paths = image
            .source_files()
            .iter()
            .map(|file| self.local_path(&file.path))
            .collect::<BTreeSet<_>>();
        for path in paths {
            let Some(sources) = self.loaded_sources.as_mut() else {
                return Ok(());
            };

            let modules = sources.entry(path.clone()).or_default();
            let new = modules.is_empty();
            modules.insert(module);
            if new {
                self.client
                    .event(
                        "loadedSource",
                        json!({"reason": "new", "source": source_json(&path)}),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    /// Tells a client keeping a list of loaded sources about the files no
    /// loaded module has once a module is gone.
    pub(super) async fn remove_loaded_sources(&mut self, module: ModuleId) -> Result<(), Closed> {
        let Some(sources) = self.loaded_sources.as_mut() else {
            return Ok(());
        };

        let mut gone = Vec::new();
        sources.retain(|path, modules| {
            if modules.remove(&module) && modules.is_empty() {
                gone.push(path.clone());
                return false;
            }

            true
        });

        for path in gone {
            self.client
                .event(
                    "loadedSource",
                    json!({"reason": "removed", "source": source_json(&path)}),
                )
                .await?;
        }

        Ok(())
    }

    pub(super) fn breakpoint_locations(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<BreakpointLocationsArguments>(arguments, "breakpointLocations arguments")?;
        let handle = self.target_handle()?;
        let path = arguments
            .source
            .path
            .ok_or_else(|| ErrorBody::new("the source has no path"))?;
        let recorded = self.recorded_path(Path::new(&path));
        let line = |line: i64| self.line_from_client(line).and_then(LineNumber::new);
        let (Some(first), Some(last)) = (
            line(arguments.line),
            line(arguments.end_line.unwrap_or(arguments.line)),
        ) else {
            return Ok(json!({"breakpoints": []}));
        };

        // The program's lines, and those of the libraries loaded now.
        let mut images = vec![Arc::clone(handle.module_image())];
        images.extend(
            self.code()
                .modules()
                .iter()
                .map(|(_, image)| Arc::clone(image)),
        );
        let mut lines = BTreeSet::new();

        for image in &images {
            if let Ok(file) = image.source_file_matching(&recorded) {
                lines.extend(image.breakpoint_lines(file.id, first..=last));
            }
        }

        let lines = lines
            .into_iter()
            .map(|line| json!({"line": self.line_to_client(line.get())}))
            .collect::<Vec<_>>();
        Ok(json!({"breakpoints": lines}))
    }

    /// The places a thread may be moved to on a line: the line itself, or
    /// the next with code, as a breakpoint there would slide.
    pub(super) fn goto_targets(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<GotoTargetsArguments>(arguments, "gotoTargets arguments")?;
        self.current_stop()?;
        let handle = self.target_handle()?;
        let path = arguments
            .source
            .path
            .ok_or_else(|| ErrorBody::new("the source has no path"))?;
        let recorded = self.recorded_path(Path::new(&path));
        let Some(line) = self
            .line_from_client(arguments.line)
            .and_then(LineNumber::new)
        else {
            return Ok(json!({"targets": []}));
        };

        let mut images = vec![Arc::clone(handle.module_image())];
        images.extend(
            self.code()
                .modules()
                .iter()
                .map(|(_, image)| Arc::clone(image)),
        );
        let lines = images
            .iter()
            .filter_map(|image| {
                let file = image.source_file_matching(&recorded).ok()?;
                image.breakpoint_line(file.id, line)
            })
            .collect::<BTreeSet<_>>();
        let mut targets = Vec::new();

        for line in lines {
            let id = self.references.goto_target(BreakpointSpec::Source {
                path: recorded.clone(),
                line,
            })?;
            targets.push(json!({
                "id": id,
                "label": format!("line {line}"),
                "line": self.line_to_client(line.get()),
            }));
        }

        Ok(json!({"targets": targets}))
    }
}

pub(super) fn module_json(record: &LoadedModuleRecord, image: Option<&ModuleImage>) -> Value {
    let mut module = json!({
        "id": record.module.id.get().to_string(),
        "name": record
            .path
            .file_name()
            .map_or_else(|| record.path.display().to_string(), |name| name.to_string_lossy().into_owned()),
        "path": record.path.display().to_string(),
    });

    if let Some(image) = image {
        let debug_information = image.functions().len() != 0;
        let status = if debug_information {
            "debug information loaded"
        } else if image.symbols().next().is_some() {
            "symbols only, no debug information"
        } else {
            "no symbols"
        };

        let mut status = match image.separate_debug_file() {
            Some(microscope::DebugFile::Unusable { path, reason }) => format!(
                "{status}; cannot use the debug file {}: {reason}",
                path.display()
            ),
            _ => status.to_owned(),
        };

        if let Some(problem) = crate::present::debug_information::dwarf_problem(image) {
            status = format!("{status}; {problem}");
        }

        module["symbolStatus"] = status.into();

        // A separate debug file, when the module's own was stripped.
        if let Some(debug_file) = image.debug_file() {
            module["symbolFilePath"] = debug_file.display().to_string().into();
        } else if debug_information {
            module["symbolFilePath"] = image.path().display().to_string().into();
        }

        let range = image.address_range();
        let bias = record.module.load_bias;
        module["addressRange"] = format!(
            "{:#x}-{:#x}",
            bias.wrapping_load(range.start),
            bias.wrapping_load(range.end)
        )
        .into();
    }

    module
}

pub fn source_json(path: &Path) -> Value {
    json!({
        "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
        "path": path.display().to_string(),
    })
}
