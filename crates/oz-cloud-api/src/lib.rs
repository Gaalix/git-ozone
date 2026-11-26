// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Open-core OSS trait seams for optional Ozone Cloud capabilities.
//!
//! All local developer and single-machine agent capabilities remain 100% open-source
//! (`MIT OR Apache-2.0`). Commercial SaaS/self-hosted extensions integrate exclusively
//! behind these traits:
//! - [`RemoteSnapshotStore`] (Ozone Pro / Team remote warm workspace streaming)
//! - [`RadarSyncClient`] (Ozone Pro / Team multiplayer collision radar)
//! - [`AuditSink`] (Ozone Enterprise SOC2 / AI provenance audit log streaming)
//! - [`PolicyProvider`] (Ozone Enterprise org-wide enforced `--scope`/`--deny` rules)

use chrono::{DateTime, Utc};
use oz_core::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

/// Dyn-compatible boxed future returned by Ozone Cloud extension traits.
pub type CloudFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Content-addressed chunk reference within a remote warm snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChunkRef {
    pub blake3_hex: String,
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
}

/// Remote warm baseline + dependency cache manifest (`oz clone --warm`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub repo_slug: String,
    pub commit_oid: String,
    pub os: String,
    pub arch: String,
    pub created_at: DateTime<Utc>,
    pub chunks: Vec<SnapshotChunkRef>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Active dirty path update published by a local agent for multiplayer radar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRadarUpdate {
    pub repo_slug: String,
    pub actor_id: String,
    pub workspace: String,
    pub branch: String,
    pub dirty_paths: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

/// Cross-machine collision alert returned by [`RadarSyncClient`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCollisionNotice {
    pub path: String,
    pub remote_actor: String,
    pub remote_workspace: String,
    pub remote_branch: String,
    pub severity: String,
}

/// Structured audit event emitted for enterprise governance and provenance tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub op_id: Option<String>,
    pub actor: String,
    pub workspace: Option<String>,
    pub action: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Organization-enforced workspace and commit guardrail policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OrgPolicy {
    #[serde(default)]
    pub enforced_deny_globs: Vec<String>,
    #[serde(default)]
    pub required_scope_globs: Vec<String>,
    pub require_signed_commits: bool,
    pub max_blob_mb_cap: Option<u64>,
}

/// Trait seam for remote warm baseline streaming (`Ozone Pro / Team`).
pub trait RemoteSnapshotStore: Send + Sync {
    fn upload_snapshot<'a>(&'a self, manifest: &'a SnapshotManifest) -> CloudFuture<'a, ()>;
    fn fetch_manifest<'a>(
        &'a self,
        repo_slug: &'a str,
        commit_oid: &'a str,
    ) -> CloudFuture<'a, Option<SnapshotManifest>>;
    fn has_snapshot<'a>(&'a self, repo_slug: &'a str, commit_oid: &'a str)
    -> CloudFuture<'a, bool>;
}

/// Trait seam for cross-machine multiplayer collision radar (`Ozone Pro / Team`).
pub trait RadarSyncClient: Send + Sync {
    fn publish_dirty_set<'a>(&'a self, update: &'a RemoteRadarUpdate) -> CloudFuture<'a, ()>;
    fn poll_collisions<'a>(
        &'a self,
        repo_slug: &'a str,
        local_paths: &'a [String],
    ) -> CloudFuture<'a, Vec<RemoteCollisionNotice>>;
}

/// Trait seam for centralized audit log streaming (`Ozone Enterprise`).
pub trait AuditSink: Send + Sync {
    fn emit_event<'a>(&'a self, event: &'a AuditEvent) -> CloudFuture<'a, ()>;
    fn flush(&self) -> CloudFuture<'_, ()>;
}

/// Trait seam for org-enforced guardrails and compliance policies (`Ozone Enterprise`).
pub trait PolicyProvider: Send + Sync {
    fn fetch_policy<'a>(&'a self, repo_slug: &'a str) -> CloudFuture<'a, OrgPolicy>;
}

/// Default no-op OSS cloud implementation used when no cloud backend is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCloudProvider;

impl RemoteSnapshotStore for NoopCloudProvider {
    fn upload_snapshot<'a>(&'a self, _manifest: &'a SnapshotManifest) -> CloudFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn fetch_manifest<'a>(
        &'a self,
        _repo_slug: &'a str,
        _commit_oid: &'a str,
    ) -> CloudFuture<'a, Option<SnapshotManifest>> {
        Box::pin(async { Ok(None) })
    }

    fn has_snapshot<'a>(
        &'a self,
        _repo_slug: &'a str,
        _commit_oid: &'a str,
    ) -> CloudFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }
}

impl RadarSyncClient for NoopCloudProvider {
    fn publish_dirty_set<'a>(&'a self, _update: &'a RemoteRadarUpdate) -> CloudFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn poll_collisions<'a>(
        &'a self,
        _repo_slug: &'a str,
        _local_paths: &'a [String],
    ) -> CloudFuture<'a, Vec<RemoteCollisionNotice>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl AuditSink for NoopCloudProvider {
    fn emit_event<'a>(&'a self, _event: &'a AuditEvent) -> CloudFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn flush(&self) -> CloudFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl PolicyProvider for NoopCloudProvider {
    fn fetch_policy<'a>(&'a self, _repo_slug: &'a str) -> CloudFuture<'a, OrgPolicy> {
        Box::pin(async { Ok(OrgPolicy::default()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_cloud_provider_is_dyn_compatible_and_callable() {
        let provider = Arc::new(NoopCloudProvider);
        let store: Arc<dyn RemoteSnapshotStore> = provider.clone();
        let radar: Arc<dyn RadarSyncClient> = provider.clone();
        let audit: Arc<dyn AuditSink> = provider.clone();
        let policy: Arc<dyn PolicyProvider> = provider;

        assert!(!store.has_snapshot("org/repo", "abc").await.expect("has"));
        assert_eq!(
            radar
                .poll_collisions("org/repo", &["src/lib.rs".to_string()])
                .await
                .expect("poll"),
            Vec::new()
        );
        audit.flush().await.expect("flush");
        assert_eq!(
            policy
                .fetch_policy("org/repo")
                .await
                .expect("policy")
                .enforced_deny_globs,
            Vec::<String>::new()
        );
    }
}
