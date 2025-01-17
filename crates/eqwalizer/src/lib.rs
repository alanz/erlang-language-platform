/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Write;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use ast::form::ExternalForm;
use ast::Error;
use elp_base_db::ModuleName;
use elp_base_db::ProjectId;
use elp_syntax::TextRange;
use fxhash::FxHashMap;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use tempfile::Builder;
use tempfile::TempPath;

pub mod ipc;
use ipc::IpcHandle;
use ipc::MsgFromEqWAlizer;
use ipc::MsgToEqWAlizer;

use crate::ipc::EqWAlizerASTFormat;

pub mod ast;

// Bundle file with command to make sure it's not removed too early
#[derive(Clone)]
pub struct Eqwalizer {
    cmd: OsString,
    args: Vec<OsString>,
    pub shell: bool,
    // Used only for the Drop implementation
    _file: Option<Arc<TempPath>>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum EqwalizerDiagnostics {
    Diagnostics(FxHashMap<String, Vec<EqwalizerDiagnostic>>),
    NoAst { module: String },
    Error(String),
}

impl Default for EqwalizerDiagnostics {
    fn default() -> Self {
        EqwalizerDiagnostics::Diagnostics(Default::default())
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EqwalizerDiagnostic {
    #[serde(deserialize_with = "deserialize_text_range")]
    pub range: TextRange,
    pub message: String,
    pub uri: String,
    pub code: String,
    #[serde(rename(deserialize = "expressionOrNull"))]
    pub expression: Option<String>,
    #[serde(rename(deserialize = "explanationOrNull"))]
    pub explanation: Option<String>,
}

impl EqwalizerDiagnostics {
    pub fn combine(mut self, other: &Self) -> Self {
        match &mut self {
            EqwalizerDiagnostics::NoAst { .. } => self,
            EqwalizerDiagnostics::Error(_) => self,
            EqwalizerDiagnostics::Diagnostics(diags) => match other {
                EqwalizerDiagnostics::Diagnostics(other_diags) => {
                    diags.extend(
                        other_diags
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v.to_vec())),
                    );
                    self
                }
                EqwalizerDiagnostics::Error(_) => other.clone(),
                EqwalizerDiagnostics::NoAst { .. } => other.clone(),
            },
        }
    }
}

#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct EqwalizerStats {
    ignores: u32,
    fixmes: u32,
    nowarn: u32,
}

pub trait DbApi {
    fn eqwalizing_start(&self, module: String) -> ();
    fn eqwalizing_done(&self, module: String) -> ();
    fn set_module_ipc_handle(&self, module: ModuleName, handle: Arc<Mutex<IpcHandle>>) -> ();
    fn module_ipc_handle(&self, module: ModuleName) -> Option<Arc<Mutex<IpcHandle>>>;
}

#[salsa::query_group(EqwalizerDiagnosticsDatabaseStorage)]
pub trait EqwalizerDiagnosticsDatabase: ast::db::EqwalizerASTDatabase + DbApi {
    fn module_diagnostics(
        &self,
        project_id: ProjectId,
        module: String,
    ) -> (Arc<EqwalizerDiagnostics>, Instant);

    fn compute_eqwalizer_stats(
        &self,
        project_id: ProjectId,
        module: ModuleName,
    ) -> Option<Arc<EqwalizerStats>>;
}

fn deserialize_text_range<'de, D>(deserializer: D) -> Result<TextRange, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct RawTextRange {
        start: u32,
        end: u32,
    }

    let range = RawTextRange::deserialize(deserializer)?;
    // Temporary for T148094436
    let _pctx = stdx::panic_context::enter(format!("\neqwalizer::deserialize_text_range"));
    Ok(TextRange::new(range.start.into(), range.end.into()))
}

impl Default for Eqwalizer {
    fn default() -> Self {
        let env = env::var("ELP_EQWALIZER_PATH");
        let (path, ext, temp_file) = if let Ok(path) = env {
            let path = PathBuf::from(path);
            let ext = path
                .extension()
                .unwrap_or_default()
                .to_str()
                .unwrap()
                .to_string();
            (path, ext, None)
        } else {
            let extension = env!("ELP_EQWALIZER_EXT").to_string();
            let eqwalizer_src = include_bytes!(concat!(env!("OUT_DIR"), "/eqwalizer"));
            let mut temp_file = Builder::new()
                .prefix("eqwalizer")
                .tempfile()
                .expect("can't create eqwalizer temp executable");
            temp_file
                .write_all(eqwalizer_src)
                .expect("can't create eqwalizer temp executable");

            let temp_file = temp_file.into_temp_path();

            let mut perm = fs::metadata(&temp_file)
                .expect("can't create eqwalizer temp executable")
                .permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&temp_file, perm).expect("can't create eqwalizer temp executable");

            (temp_file.to_path_buf(), extension, Some(temp_file))
        };

        let (cmd, args) = match ext.as_str() {
            "jar" => (
                "java".into(),
                vec!["-Xss20M".into(), "-jar".into(), path.into()],
            ),
            "" => (path.into(), vec![]),
            _ => panic!("Unknown eqwalizer executable {:?}", path),
        };

        Self {
            cmd,
            args,
            shell: false,
            _file: temp_file.map(Arc::new),
        }
    }
}

impl Eqwalizer {
    // Return a smart pointer to bundle lifetime with the temp file's lifetime
    pub fn cmd<'file>(&'file self) -> CommandProxy<'file> {
        let mut cmd = Command::new(&self.cmd);
        cmd.args(&self.args);
        CommandProxy::new(cmd)
    }

    pub fn typecheck(
        &self,
        build_info_path: &Path,
        db: &dyn EqwalizerDiagnosticsDatabase,
        project_id: ProjectId,
        modules: Vec<&str>,
    ) -> EqwalizerDiagnostics {
        let mut cmd = self.cmd();
        cmd.arg("ipc");
        cmd.args(modules);
        cmd.env("EQWALIZER_IPC", "true");
        cmd.env("EQWALIZER_USE_ELP_CONVERTED_AST", "true");
        if self.shell {
            cmd.env("EQWALIZER_ELP_SHELL", "true");
        }
        add_env(&mut cmd, build_info_path, None);

        if self.shell {
            match shell_typecheck(cmd, db, project_id) {
                Ok(diags) => diags,
                Err(err) => EqwalizerDiagnostics::Error(format!("{}", err)),
            }
        } else {
            match do_typecheck(cmd, db, project_id) {
                Ok(diags) => diags,
                Err(err) => EqwalizerDiagnostics::Error(format!("{}", err)),
            }
        }
    }

    pub fn passthrough(
        &self,
        args: &[String],
        build_info_path: &Path,
        elp_ast_dir: &Path,
    ) -> Result<ExitStatus> {
        let mut cmd = self.cmd();
        cmd.args(args);
        add_env(&mut cmd, build_info_path, Some(elp_ast_dir));
        cmd.status()
            .with_context(|| "Error in eqwalizer passthrough")
    }
}

fn do_typecheck(
    mut cmd: CommandProxy,
    db: &dyn EqwalizerDiagnosticsDatabase,
    project_id: ProjectId,
) -> Result<EqwalizerDiagnostics, anyhow::Error> {
    let mut handle = IpcHandle::from_command(&mut cmd)
        .with_context(|| format!("starting eqWAlizer process: {:?}", cmd))?;
    let _pctx = stdx::panic_context::enter(format!("\neqWAlizing with command: {:?}", cmd));
    loop {
        db.unwind_if_cancelled();
        match handle.receive()? {
            MsgFromEqWAlizer::GetAstBytes { module, format } => {
                log::debug!(
                    "received from eqwalizer: GetAstBytes for module {} (format = {:?})",
                    module,
                    format
                );
                let module_name = ModuleName::new(&module);
                let ast = {
                    match format {
                        EqWAlizerASTFormat::RawForms => {
                            db.get_erl_ast_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ConvertedForms => {
                            db.converted_ast_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::RawStub => {
                            db.get_erl_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ConvertedStub => {
                            db.converted_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ExpandedStub => {
                            db.expanded_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ContractiveStub => {
                            db.contractive_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::CovariantStub => {
                            db.covariant_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::TransitiveStub => {
                            db.transitive_stub_bytes(project_id, module_name)
                        }
                    }
                };
                match ast {
                    Ok(ast_bytes) => {
                        log::debug!(
                            "sending to eqwalizer: GetAstBytesReply for module {}",
                            module
                        );
                        let ast_bytes_len = ast_bytes.len().try_into()?;
                        let reply = &MsgToEqWAlizer::GetAstBytesReply { ast_bytes_len };
                        handle.send(reply)?;
                        handle.receive_newline()?;
                        handle.send_bytes(&ast_bytes)?;
                    }
                    Err(Error::ModuleNotFound(_)) => {
                        log::debug!(
                            "module not found, sending to eqwalizer: empty GetAstBytesReply for module {}",
                            module
                        );
                        let ast_bytes_len = 0;
                        let reply = &MsgToEqWAlizer::GetAstBytesReply { ast_bytes_len };
                        handle.send(reply)?;
                        handle.receive_newline()?;
                    }
                    Err(Error::ParseError) => {
                        log::debug!(
                            "parse error, sending to eqwalizer: CannotCompleteRequest for module {}",
                            module
                        );
                        let reply = &MsgToEqWAlizer::CannotCompleteRequest;
                        handle.send(reply)?;
                        return Ok(EqwalizerDiagnostics::NoAst { module });
                    }
                    Err(err) => {
                        log::debug!(
                            "error {} sending to eqwalizer: CannotCompleteRequest for module {}",
                            err,
                            module
                        );
                        let reply = &MsgToEqWAlizer::CannotCompleteRequest;
                        handle.send(reply)?;
                        return Ok(EqwalizerDiagnostics::Error(err.to_string()));
                    }
                }
            }
            MsgFromEqWAlizer::EqwalizingStart { module } => db.eqwalizing_start(module),
            MsgFromEqWAlizer::EqwalizingDone { module } => db.eqwalizing_done(module),
            MsgFromEqWAlizer::Done { diagnostics } => {
                log::debug!(
                    "received from eqwalizer: Done with diagnostics length {}",
                    diagnostics.len()
                );
                return Ok(EqwalizerDiagnostics::Diagnostics(diagnostics));
            }
            msg => {
                log::warn!(
                    "received unexpected message from eqwalizer, ignoring: {:?}",
                    msg
                )
            }
        }
    }
}

fn shell_typecheck(
    mut cmd: CommandProxy,
    db: &dyn EqwalizerDiagnosticsDatabase,
    project_id: ProjectId,
) -> Result<EqwalizerDiagnostics, anyhow::Error> {
    // Never cache the results of this function
    db.salsa_runtime().report_untracked_read();
    let handle = Arc::new(Mutex::new(
        IpcHandle::from_command(&mut cmd)
            .with_context(|| format!("starting eqWAlizer process: {:?}", cmd))?,
    ));
    let mut diagnostics = EqwalizerDiagnostics::default();
    loop {
        db.unwind_if_cancelled();
        let msg = handle.lock().receive()?;
        match msg {
            MsgFromEqWAlizer::EnteringModule { module } => {
                db.set_module_ipc_handle(ModuleName::new(&module), handle.clone());
                let diags = db.module_diagnostics(project_id, module).0;
                handle.lock().send(&MsgToEqWAlizer::ELPExitingModule)?;
                diagnostics = diagnostics.combine(&diags);
            }
            MsgFromEqWAlizer::Done { .. } => {
                return Ok(diagnostics);
            }
            msg => {
                log::warn!(
                    "received unexpected message from eqwalizer, ignoring: {:?}",
                    msg
                )
            }
        }
    }
}

fn module_diagnostics(
    db: &dyn EqwalizerDiagnosticsDatabase,
    project_id: ProjectId,
    module: String,
) -> (Arc<EqwalizerDiagnostics>, Instant) {
    // A timestamp is added to the return value to force Salsa to store new
    // diagnostics, and not attempt to back-date them if they are equal to
    // the memoized ones.
    let timestamp = Instant::now();
    match get_module_diagnostics(db, project_id, module) {
        Ok(diag) => (Arc::new(diag), timestamp),
        Err(err) => (
            Arc::new(EqwalizerDiagnostics::Error(format!("{}", err))),
            timestamp,
        ),
    }
}

fn get_module_diagnostics(
    db: &dyn EqwalizerDiagnosticsDatabase,
    project_id: ProjectId,
    module: String,
) -> Result<EqwalizerDiagnostics, anyhow::Error> {
    let handle_mutex = db
        .module_ipc_handle(ModuleName::new(&module))
        .ok_or(anyhow::Error::msg(format!(
            "no eqWAlizer handle for module {}",
            module
        )))?;
    let mut handle = handle_mutex.lock();
    handle.send(&MsgToEqWAlizer::ELPEnteringModule)?;
    loop {
        db.unwind_if_cancelled();
        match handle.receive()? {
            MsgFromEqWAlizer::GetAstBytes { module, format } => {
                log::debug!(
                    "received from eqwalizer: GetAstBytes for module {} (format = {:?})",
                    module,
                    format
                );
                let module_name = ModuleName::new(&module);
                let ast = {
                    match format {
                        EqWAlizerASTFormat::RawForms => {
                            db.get_erl_ast_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ConvertedForms => {
                            db.converted_ast_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::RawStub => {
                            db.get_erl_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ConvertedStub => {
                            db.converted_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ExpandedStub => {
                            db.expanded_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::ContractiveStub => {
                            db.contractive_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::CovariantStub => {
                            db.covariant_stub_bytes(project_id, module_name)
                        }
                        EqWAlizerASTFormat::TransitiveStub => {
                            db.transitive_stub_bytes(project_id, module_name)
                        }
                    }
                };
                match ast {
                    Ok(ast_bytes) => {
                        log::debug!(
                            "sending to eqwalizer: GetAstBytesReply for module {}",
                            module
                        );
                        let ast_bytes_len = ast_bytes.len().try_into()?;
                        let reply = &MsgToEqWAlizer::GetAstBytesReply { ast_bytes_len };
                        handle.send(reply)?;
                        handle.receive_newline()?;
                        handle.send_bytes(&ast_bytes)?;
                    }
                    Err(Error::ModuleNotFound(_)) => {
                        log::debug!(
                            "module not found, sending to eqwalizer: empty GetAstBytesReply for module {}",
                            module
                        );
                        let ast_bytes_len = 0;
                        let reply = &MsgToEqWAlizer::GetAstBytesReply { ast_bytes_len };
                        handle.send(reply)?;
                        handle.receive_newline()?;
                    }
                    Err(Error::ParseError) => {
                        log::debug!(
                            "parse error, sending to eqwalizer: CannotCompleteRequest for module {}",
                            module
                        );
                        let reply = &MsgToEqWAlizer::CannotCompleteRequest;
                        handle.send(reply)?;
                        return Ok(EqwalizerDiagnostics::NoAst { module });
                    }
                    Err(err) => {
                        log::debug!(
                            "error {} sending to eqwalizer: CannotCompleteRequest for module {}",
                            err,
                            module
                        );
                        let reply = &MsgToEqWAlizer::CannotCompleteRequest;
                        handle.send(reply)?;
                        return Ok(EqwalizerDiagnostics::Error(err.to_string()));
                    }
                }
            }
            MsgFromEqWAlizer::EqwalizingStart { module } => db.eqwalizing_start(module),
            MsgFromEqWAlizer::EqwalizingDone { module } => db.eqwalizing_done(module),
            MsgFromEqWAlizer::Done { diagnostics } => {
                log::debug!(
                    "received from eqwalizer: Done with diagnostics length {}",
                    diagnostics.len()
                );
                return Ok(EqwalizerDiagnostics::Diagnostics(diagnostics));
            }
            MsgFromEqWAlizer::Dependencies { modules } => {
                modules.iter().for_each(|module| {
                    let module = ModuleName::new(&module);
                    _ = db.transitive_stub_bytes(project_id, module);
                });
            }
            msg => {
                log::warn!(
                    "received unexpected message from eqwalizer, ignoring: {:?}",
                    msg
                )
            }
        }
    }
}

fn compute_eqwalizer_stats(
    db: &dyn EqwalizerDiagnosticsDatabase,
    project_id: ProjectId,
    module: ModuleName,
) -> Option<Arc<EqwalizerStats>> {
    let ast = db.converted_ast(project_id, module).ok()?;
    let mut fixmes = 0;
    let mut ignores = 0;
    let mut nowarn = 0;
    for form in ast.to_vec() {
        match form {
            ExternalForm::ElpMetadata(meta) => {
                for fixme in meta.fixmes {
                    if fixme.is_ignore {
                        ignores += 1
                    } else {
                        fixmes += 1
                    }
                }
            }
            ExternalForm::EqwalizerNowarnFunction(_) => nowarn += 1,
            _ => (),
        }
    }
    if fixmes == 0 && ignores == 0 && nowarn == 0 {
        return None;
    }
    Some(Arc::new(EqwalizerStats {
        fixmes,
        ignores,
        nowarn,
    }))
}

fn add_env(cmd: &mut Command, build_info_path: &Path, elp_ast_dir: Option<&Path>) {
    cmd.env("EQWALIZER_BUILD_INFO", build_info_path);
    if let Some(elp_ast_dir) = elp_ast_dir {
        cmd.env("EQWALIZER_ELP_AST_DIR", elp_ast_dir);
    }
}

/// This ensures the enclosed Command struct won't outlive the related temp file
pub struct CommandProxy<'file>(Command, PhantomData<&'file TempPath>);

impl<'file> CommandProxy<'file> {
    pub fn new(cmd: Command) -> Self {
        Self(cmd, PhantomData)
    }
}

impl<'file> Deref for CommandProxy<'file> {
    type Target = Command;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'file> DerefMut for CommandProxy<'file> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl fmt::Debug for CommandProxy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
