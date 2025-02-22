/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

// Eden's Thrift API does sometime want &Vec<...>.
#![allow(clippy::useless_vec)]

use allocative::Allocative;
use anyhow::Context as _;
use async_trait::async_trait;
use buck2_core;
use buck2_core::env_helper::EnvHelper;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::io_counters::IoCounterKey;
use compact_str::CompactString;
use dupe::Dupe;
use edenfs::types::FileAttributes;
use edenfs::types::ReaddirParams;
use edenfs::types::SourceControlType;
use edenfs::types::SyncBehavior;
use edenfs::types::SynchronizeWorkingCopyParams;
use fbinit::FacebookInit;
use libc::EINVAL;
use libc::EISDIR;
use libc::ENOENT;
use libc::ENOTDIR;
use tokio::sync::Semaphore;

use crate::cas_digest::CasDigestConfig;
use crate::eden::EdenConnectionManager;
use crate::eden::EdenDataIntoResult;
use crate::file_ops::FileType;
use crate::file_ops::RawDirEntry;
use crate::file_ops::RawPathMetadata;
use crate::io::fs::FsIoProvider;
use crate::io::IoProvider;

#[derive(Allocative)]
pub struct EdenIoProvider {
    manager: EdenConnectionManager,
    fs: FsIoProvider,
}

impl EdenIoProvider {
    pub async fn new(
        fb: FacebookInit,
        fs: &ProjectRoot,
        cas_digest_config: CasDigestConfig,
    ) -> anyhow::Result<Option<Self>> {
        if cfg!(not(fbcode_build)) {
            tracing::warn!("Disabling Eden I/O: Cargo build detected");
            return Ok(None);
        }

        const MINIMUM_SUPPORTED_EDEN_VERSION: &str = "20220720-094125";

        static EDEN_SEMAPHORE: EnvHelper<usize> = EnvHelper::new("BUCK2_EDEN_SEMAPHORE");
        let eden_semaphore = EDEN_SEMAPHORE.get_copied()?.unwrap_or(2048);

        let manager =
            match EdenConnectionManager::new(fb, fs.root(), Semaphore::new(eden_semaphore))? {
                Some(manager) => manager,
                None => return Ok(None),
            };

        let eden_version = manager
            .get_eden_version()
            .await
            .context("Error querying Eden version")?;

        if let Some(eden_version) = &eden_version {
            if eden_version.as_str() < MINIMUM_SUPPORTED_EDEN_VERSION {
                tracing::warn!(
                    "Disabling Eden I/O: \
                    your Eden version ({}) is too old to support Eden I/O (minimum required: {}). \
                    Update Eden then restart Buck2 to use Eden I/O.",
                    eden_version,
                    MINIMUM_SUPPORTED_EDEN_VERSION
                );
                return Ok(None);
            }
        } else {
            tracing::warn!("You are using a development version of Eden, enabling Eden I/O");
        }

        if !cas_digest_config.allows_sha1() {
            tracing::warn!("Disabling Eden I/O: your digest config does not allow SHA1");
            return Ok(None);
        }

        Ok(Some(Self {
            manager,
            fs: FsIoProvider::new(fs.dupe(), cas_digest_config),
        }))
    }
}

#[async_trait]
impl IoProvider for EdenIoProvider {
    async fn read_file_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> anyhow::Result<Option<String>> {
        self.fs.read_file_if_exists(path).await
    }

    async fn read_dir(&self, path: ProjectRelativePathBuf) -> anyhow::Result<Vec<RawDirEntry>> {
        let _guard = IoCounterKey::ReadDirEden.guard();

        let requested_attributes = i64::from(i32::from(FileAttributes::SOURCE_CONTROL_TYPE));

        let params = ReaddirParams {
            mountPoint: self.manager.get_mount_point(),
            directoryPaths: vec![path.to_string().into_bytes()],
            requestedAttributes: requested_attributes,
            sync: no_sync(),
            ..Default::default()
        };

        let res = self
            .manager
            .with_eden(|eden| {
                tracing::trace!("readdir({})", path);
                eden.readdir(&params)
            })
            .await?
            .dirLists;

        let data = res
            .into_iter()
            .next()
            .context("Eden did not return a directory result")?
            .into_result()?;

        tracing::debug!("readdir({}): {} entries", path, data.len(),);

        let entries = data
            .into_iter()
            .map(|(file_name, attrs)| {
                let file_name =
                    CompactString::from_utf8(file_name).context("Filename is not UTF-8")?;

                let source_control_type = attrs
                    .into_result()?
                    .sourceControlType
                    .context("Missing sourceControlType")?
                    .into_result()?;

                let file_type = match source_control_type {
                    SourceControlType::TREE => FileType::Directory,
                    SourceControlType::REGULAR_FILE | SourceControlType::EXECUTABLE_FILE => {
                        FileType::File
                    }
                    SourceControlType::SYMLINK => FileType::Symlink,
                    _ => FileType::Unknown,
                };

                anyhow::Ok(RawDirEntry {
                    file_name,
                    file_type,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    async fn read_path_metadata_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> anyhow::Result<Option<RawPathMetadata<ProjectRelativePathBuf>>> {
        use edenfs::types::GetAttributesFromFilesParams;

        use crate::eden::EdenError;
        use crate::file_ops::FileDigest;
        use crate::file_ops::FileMetadata;
        use crate::file_ops::TrackedFileDigest;

        let _guard = IoCounterKey::StatEden.guard();

        let requested_attributes = i64::from(
            i32::from(FileAttributes::SHA1_HASH)
                | i32::from(FileAttributes::FILE_SIZE)
                | i32::from(FileAttributes::SOURCE_CONTROL_TYPE),
        );

        let params = GetAttributesFromFilesParams {
            mountPoint: self.manager.get_mount_point(),
            paths: vec![path.to_string().into_bytes()],
            requestedAttributes: requested_attributes,
            sync: no_sync(),
            ..Default::default()
        };

        let attrs = self
            .manager
            .with_eden(|eden| {
                tracing::trace!("getAttributesFromFiles({})", path);
                eden.getAttributesFromFiles(&params)
            })
            .await?;

        match attrs
            .res
            .into_iter()
            .next()
            .context("Eden did not return file info")?
            .into_result()
        {
            Ok(data) => {
                tracing::debug!("getAttributesFromFiles({}): ok", path,);
                let digest = FileDigest::new_sha1(
                    data.sha1
                        .context("Eden did not return a sha1")?
                        .try_into()
                        .ok()
                        .context("Eden returned an invalid sha1")?,
                    data.fileSize
                        .context("Eden did not return a fileSize")?
                        .try_into()
                        .context("Eden returned an invalid fileSize")?,
                );

                // TODO (DigestConfig): Check that the config allows SHA1 earlier here.
                let digest = TrackedFileDigest::new(digest, self.fs.cas_digest_config());

                let is_executable = data.r#type.context("Eden did not return a type")?
                    == SourceControlType::EXECUTABLE_FILE;

                let meta = FileMetadata {
                    digest,
                    is_executable,
                };

                Ok(Some(RawPathMetadata::File(meta)))
            }
            Err(EdenError::PosixError { code, .. }) if code == EISDIR => {
                tracing::debug!("getAttributesFromFiles({}): EISDIR", path);
                Ok(Some(RawPathMetadata::Directory))
            }
            Err(EdenError::PosixError { code, .. }) if code == ENOENT => {
                tracing::debug!("getAttributesFromFiles({}): ENOENT", path);
                Ok(None)
            }
            Err(EdenError::PosixError { code, .. }) if code == EINVAL || code == ENOTDIR => {
                // If we get EINVAL it means the target wasn't a file, and since we know it
                // existed and it wasn't a dir, then that means it must be a symlink. If we get
                // ENOTDIR, that means we tried to traverse a path component that was a
                // symlink. In both cases, we need to both a) handle ExternalSymlink and b)
                // look through to the target, so we do that.
                // TODO: It would be better to read the link then ask Eden for the SHA1.
                tracing::debug!("getAttributesFromFiles({}): fallthrough", path);
                self.fs.read_path_metadata_if_exists(path).await
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn settle(&self) -> anyhow::Result<()> {
        let _guard = IoCounterKey::EdenSettle.guard();

        let root = self.manager.get_mount_point();

        self.manager
            .with_eden(move |eden| {
                eden.synchronizeWorkingCopy(
                    &root,
                    &SynchronizeWorkingCopyParams {
                        sync: SyncBehavior {
                            syncTimeoutSeconds: None,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            })
            .await
            .context("Error synchronizing Eden working copy")
    }

    fn name(&self) -> &'static str {
        "eden"
    }

    async fn eden_version(&self) -> anyhow::Result<Option<String>> {
        self.manager.get_eden_version().await
    }

    fn project_root(&self) -> &ProjectRoot {
        self.fs.project_root()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// We don't request sync on individual calls because we make a dedicated call for this before we
/// start an operation.
fn no_sync() -> SyncBehavior {
    // NOTE/ we'd rather make this a `const` if we could, but we can't, because
    // `Default::default()` is not a const-fn.
    SyncBehavior {
        syncTimeoutSeconds: Some(0),
        ..Default::default()
    }
}
