//! Registered modules, their ops, and the lanes they declare. Startup-time validation:
//! collisions are a startup failure, not a runtime surprise (protocol.md).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use shimmer_core::ids::is_valid_name;
use shimmer_core::{CommandSpec, Error, Execution, LaneConfig, LaneId, Manifest, Module, ModuleId, Result};

use crate::config::Config;

/// Owned by the daemon; a module may not claim them as its id.
pub const RESERVED_PREFIXES: [&str; 3] = ["core", "queue", "scheduler"];

pub struct Entry {
    pub module: Arc<dyn Module>,
    pub manifest: Manifest,
    pub commands: Vec<CommandSpec>,
    /// False until `init` succeeds. Ops on an uninitialised module answer `unavailable`.
    pub ready: AtomicBool,
}

pub struct Registry {
    pub entries: Vec<Entry>,
    by_id: HashMap<ModuleId, usize>,
    /// op -> (entry index, command index)
    by_op: HashMap<String, (usize, usize)>,
}

impl Registry {
    pub fn build(modules: Vec<Arc<dyn Module>>) -> Result<Self> {
        let mut reg = Self { entries: Vec::new(), by_id: HashMap::new(), by_op: HashMap::new() };
        let mut namespaces: HashMap<String, ModuleId> = HashMap::new();
        for module in modules {
            let manifest = module.manifest();
            let id = manifest.id.clone();
            if !is_valid_name(id.as_str()) || RESERVED_PREFIXES.contains(&id.as_str()) {
                return Err(Error::invalid_params(format!("module id '{id}' is invalid or reserved")));
            }
            if !is_valid_name(&manifest.namespace) || RESERVED_PREFIXES.contains(&manifest.namespace.as_str()) {
                return Err(Error::invalid_params(format!(
                    "module '{id}': invalid namespace '{}'",
                    manifest.namespace
                )));
            }
            if reg.by_id.contains_key(&id) {
                return Err(Error::conflict(format!("module '{id}' registered twice")));
            }
            if let Some(other) = namespaces.insert(manifest.namespace.clone(), id.clone()) {
                return Err(Error::conflict(format!(
                    "modules '{other}' and '{id}' both own namespace '{}'",
                    manifest.namespace
                )));
            }
            let prefix = format!("{id}.");
            if let Some(t) = manifest.topics.iter().find(|t| !t.starts_with(&prefix)) {
                return Err(Error::invalid_params(format!("module '{id}' declares foreign topic '{t}'")));
            }
            if let Some(t) = manifest.topic_descriptions.keys().find(|t| !manifest.topics.contains(t)) {
                return Err(Error::invalid_params(format!(
                    "module '{id}' describes topic '{t}', which it doesn't declare"
                )));
            }
            let commands = module.commands();
            let idx = reg.entries.len();
            for (ci, c) in commands.iter().enumerate() {
                if !c.op.starts_with(&prefix) || c.op.len() == prefix.len() {
                    return Err(Error::invalid_params(format!(
                        "module '{id}' declares op '{}' outside its '{id}.<verb>' namespace",
                        c.op
                    )));
                }
                if reg.by_op.insert(c.op.clone(), (idx, ci)).is_some() {
                    return Err(Error::conflict(format!("op '{}' declared twice", c.op)));
                }
            }
            reg.by_id.insert(id, idx);
            reg.entries.push(Entry { module, manifest, commands, ready: AtomicBool::new(false) });
        }
        Ok(reg)
    }

    pub fn entry(&self, id: &ModuleId) -> Option<&Entry> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    pub fn command(&self, op: &str) -> Option<(&Entry, &CommandSpec)> {
        self.by_op.get(op).map(|&(e, c)| (&self.entries[e], &self.entries[e].commands[c]))
    }

    /// Merge `default`, every module's lanes and `config.toml` overrides. The queue never
    /// hardcodes a module name (§12 rule 7): the only lane it knows is `default`.
    pub fn lanes(&self, config: &Config) -> Result<Vec<LaneConfig>> {
        let mut lanes: BTreeMap<LaneId, usize> = BTreeMap::new();
        lanes.insert(LaneId::default_lane(), 4);
        for e in &self.entries {
            for l in e.module.lanes() {
                if l.max_concurrent == 0 {
                    return Err(Error::invalid_params(format!("lane '{}' needs max_concurrent >= 1", l.id)));
                }
                if let Some(prev) = lanes.insert(l.id.clone(), l.max_concurrent) {
                    if prev != l.max_concurrent && l.id != LaneId::default_lane() {
                        return Err(Error::conflict(format!(
                            "lane '{}' declared with conflicting max_concurrent ({prev} vs {})",
                            l.id, l.max_concurrent
                        )));
                    }
                }
            }
        }
        for (name, n) in &config.lanes {
            match lanes.get_mut(&LaneId::new(name.as_str())) {
                Some(slot) => *slot = *n,
                None => tracing::warn!(lane = %name, "config.toml overrides a lane no module declares; ignored"),
            }
        }
        for e in &self.entries {
            for c in &e.commands {
                if let Execution::Queued { lane } = &c.execution {
                    if !lanes.contains_key(lane) {
                        return Err(Error::lane_unknown(format!("op '{}' uses undeclared lane '{lane}'", c.op)));
                    }
                }
            }
        }
        Ok(lanes.into_iter().map(|(id, max_concurrent)| LaneConfig { id, max_concurrent }).collect())
    }
}
