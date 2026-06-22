/*
Copyright 2024 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Extension point for CRI-triggered restore intent parsing.
//!
//! Each `RestoreIntentResolver` implementation handles one annotation key prefix and
//! translates sandbox pod annotations into a caller-independent `RestoreIntent`.
//! Resolvers are tried in order; the first to return `Some(intent)` wins.

use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::{
    sandbox::{AnnotationResolverConfig, SnapshotConfig},
    template::{
        SnapshotType, TemplateKey, WorkloadIdentity, POD_UID_ANNOTATION, SNAPSHOT_TYPE_ANNOTATION,
        TEMPLATE_ID_ANNOTATION, TEMPLATE_KEY_ANNOTATION, WORKLOAD_GENERATION_ANNOTATION,
    },
};

/// Caller-independent restore decision produced by a `RestoreIntentResolver`.
pub enum RestoreIntent {
    WarmFork {
        key: TemplateKey,
        template_id: Option<String>,
    },
    Continuation {
        identity: WorkloadIdentity,
    },
    None,
}

/// Extension point for CRI-triggered restore intent parsing.
///
/// Each implementation handles one annotation key prefix and translates it into
/// a caller-independent `RestoreIntent`. Resolvers are tried in order; the
/// first to return `Some(intent)` wins.
#[async_trait]
pub trait RestoreIntentResolver: Send + Sync {
    fn name(&self) -> &str;

    /// - `Ok(Some(intent))`: this resolver owns these annotations and produced a decision.
    /// - `Ok(None)`: this resolver does not own these annotations; try next.
    /// - `Err(_)`: this resolver owns these annotations but they are invalid; abort.
    async fn resolve(
        &self,
        annotations: &HashMap<String, String>,
        config: &SnapshotConfig,
    ) -> Result<Option<RestoreIntent>>;
}

/// Handles `kuasar.io/*` annotations (logic migrated from `parse_snapshot_intent()`).
pub struct KuasarNativeResolver;

#[async_trait]
impl RestoreIntentResolver for KuasarNativeResolver {
    fn name(&self) -> &str {
        "kuasar-native"
    }

    async fn resolve(
        &self,
        annotations: &HashMap<String, String>,
        config: &SnapshotConfig,
    ) -> Result<Option<RestoreIntent>> {
        let explicit_type = annotations
            .get(SNAPSHOT_TYPE_ANNOTATION)
            .map(|v| SnapshotType::from_annotation(v))
            .transpose()?;

        // If no relevant annotations present, yield to next resolver.
        let has_kuasar_annotations = explicit_type.is_some()
            || annotations.contains_key(TEMPLATE_ID_ANNOTATION)
            || annotations.contains_key(TEMPLATE_KEY_ANNOTATION)
            || annotations.contains_key(POD_UID_ANNOTATION);
        if !has_kuasar_annotations {
            return Ok(None);
        }

        let (template_id, template_key) = if matches!(
            explicit_type,
            Some(SnapshotType::Environment) | Some(SnapshotType::Continuation)
        ) {
            (None, None)
        } else {
            (
                annotations.get(TEMPLATE_ID_ANNOTATION).cloned(),
                annotations.get(TEMPLATE_KEY_ANNOTATION).cloned(),
            )
        };

        // Continuation path
        if !matches!(
            explicit_type,
            Some(SnapshotType::Environment) | Some(SnapshotType::WarmFork)
        ) && config.enable_continuation_restore
        {
            if let Some(pod_uid) = annotations.get(POD_UID_ANNOTATION) {
                let generation = match annotations.get(WORKLOAD_GENERATION_ANNOTATION) {
                    None => 0u64,
                    Some(s) => match s.parse::<u64>() {
                        Ok(v) => v,
                        Err(_) => {
                            return Err(anyhow!(
                                "annotation {}={:?} is not a valid u64",
                                WORKLOAD_GENERATION_ANNOTATION,
                                s
                            ));
                        }
                    },
                };
                return Ok(Some(RestoreIntent::Continuation {
                    identity: WorkloadIdentity {
                        pod_uid: pod_uid.clone(),
                        generation,
                    },
                }));
            }
        }

        // WarmFork path
        if config.enable_warmfork_restore {
            if let Some(tid) = template_id {
                let key = template_key.map(TemplateKey::user);
                return Ok(Some(RestoreIntent::WarmFork {
                    key: key.unwrap_or_else(|| TemplateKey::user(tid.clone())),
                    template_id: Some(tid),
                }));
            }
            if let Some(k) = template_key {
                return Ok(Some(RestoreIntent::WarmFork {
                    key: TemplateKey::user(k),
                    template_id: None,
                }));
            }
        }

        Ok(Some(RestoreIntent::None))
    }
}

/// Configurable annotation-to-restore-intent resolver driven by `SnapshotConfig::annotation_resolvers`.
///
/// Each entry in the config defines one mapping rule. Rules are evaluated in order;
/// the first matching annotation key wins. This allows CRI-path integrations with
/// custom annotation keys to be supported through config changes alone.
pub struct MappingResolver {
    entries: Vec<AnnotationResolverConfig>,
}

impl MappingResolver {
    pub fn new(entries: Vec<AnnotationResolverConfig>) -> Self {
        Self { entries }
    }
}

#[async_trait]
impl RestoreIntentResolver for MappingResolver {
    fn name(&self) -> &str {
        "mapping"
    }

    async fn resolve(
        &self,
        annotations: &HashMap<String, String>,
        config: &SnapshotConfig,
    ) -> Result<Option<RestoreIntent>> {
        for entry in &self.entries {
            if let Some(key) = annotations.get(&entry.snapshot_key_annotation) {
                if config.enable_warmfork_restore {
                    return Ok(Some(RestoreIntent::WarmFork {
                        key: TemplateKey::user(key.clone()),
                        template_id: None,
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Run resolvers in order; return first non-None result.
pub async fn resolve_chain(
    resolvers: &[Arc<dyn RestoreIntentResolver>],
    annotations: &HashMap<String, String>,
    config: &SnapshotConfig,
) -> Result<RestoreIntent> {
    for r in resolvers {
        match r.resolve(annotations, config).await {
            Ok(Some(intent)) => {
                log::info!("restore intent resolved by '{}'", r.name());
                return Ok(intent);
            }
            Ok(None) => continue,
            Err(e) => {
                return Err(anyhow!("resolver '{}' returned error: {}", r.name(), e));
            }
        }
    }
    Ok(RestoreIntent::None)
}
