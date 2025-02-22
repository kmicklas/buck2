/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use async_trait::async_trait;
use buck2_cli_proto::InstallRequest;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::command_outcome::CommandOutcome;
use buck2_client_ctx::common::CommonBuildConfigurationOptions;
use buck2_client_ctx::common::CommonBuildOptions;
use buck2_client_ctx::common::CommonCommandOptions;
use buck2_client_ctx::common::CommonConsoleOptions;
use buck2_client_ctx::common::CommonDaemonCommandOptions;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::daemon::client::NoPartialResultHandler;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::streaming::StreamingCommand;
use gazebo::prelude::*;

#[derive(Debug, clap::Parser)]
#[clap(name = "install", about = "Build and install an application")]
pub struct InstallCommand {
    #[clap(flatten)]
    common_opts: CommonCommandOptions,

    #[clap(flatten)]
    build_opts: CommonBuildOptions,

    #[clap(
        long,
        name = "installer-debug",
        help = "Prints installer output to stderr. It might break superconsole"
    )]
    installer_debug: bool,

    #[clap(name = "TARGET", help = "Target to build and install")]
    patterns: Vec<String>,

    #[clap(
        name = "INSTALL_ARGS",
        help = "Additional arguments passed to the install when running it",
        raw = true
    )]
    extra_run_args: Vec<String>,
}

#[async_trait]
impl StreamingCommand for InstallCommand {
    const COMMAND_NAME: &'static str = "install";
    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: &clap::ArgMatches,
        ctx: &mut ClientCommandContext<'_>,
    ) -> ExitResult {
        let context = ctx.client_context(
            &self.common_opts.config_opts,
            matches,
            ctx.sanitized_argv.argv.clone(),
        )?;
        let response = buckd
            .with_flushing()
            .install(
                InstallRequest {
                    context: Some(context),
                    target_patterns: self.patterns.map(|pat| buck2_data::TargetPattern {
                        value: pat.to_owned(),
                    }),
                    build_opts: Some(self.build_opts.to_proto()),
                    installer_run_args: self.extra_run_args,
                    installer_debug: self.installer_debug,
                },
                ctx.stdin()
                    .console_interaction_stream(&self.common_opts.console_opts),
                &mut NoPartialResultHandler,
            )
            .await;
        let console = self.common_opts.console_opts.final_console();

        match response {
            Ok(CommandOutcome::Success(_)) => {
                console.print_success("INSTALL SUCCEEDED")?;
                ExitResult::success()
            }
            Ok(CommandOutcome::Failure(_)) | Err(_) => {
                console.print_error("INSTALL FAILED")?;
                ExitResult::failure()
            }
        }
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        &self.common_opts.console_opts
    }

    fn event_log_opts(&self) -> &CommonDaemonCommandOptions {
        &self.common_opts.event_log_opts
    }

    fn common_opts(&self) -> &CommonBuildConfigurationOptions {
        &self.common_opts.config_opts
    }
}
