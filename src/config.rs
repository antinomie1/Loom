// SPDX-License-Identifier: BSD-2-Clause

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::Deserialize;

use crate::model::{
    ConfigError, Dependencies, ManagerScope, ServiceDefinition, ServiceId, require_schema,
};

const MAX_STARTING_HARD_LIMIT: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupDefinition {
    pub id: ServiceId,
    pub dependencies: Dependencies,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigWarning {
    MissingWanted { owner: ServiceId, target: ServiceId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigSnapshot {
    services: BTreeMap<ServiceId, ServiceDefinition>,
    groups: BTreeMap<ServiceId, GroupDefinition>,
    default_group: ServiceId,
    max_starting: usize,
    warnings: Vec<ConfigWarning>,
}

impl ConfigSnapshot {
    /// Builds a complete configuration snapshot from already layered sources.
    ///
    /// Each service tuple contains the identifier derived from its filename and
    /// the full TOML document. Filesystem ownership and layering are handled by
    /// the loader; this function owns schema and graph validation.
    ///
    /// # Errors
    ///
    /// Returns an error when any document is invalid, a definition is duplicated,
    /// the graph has a missing strict target or cycle, or a group activates a
    /// conflict.
    pub fn build<'a>(
        manager_source: &str,
        service_sources: impl IntoIterator<Item = (ServiceId, &'a str)>,
        scope: ManagerScope,
    ) -> Result<Self, ConfigError> {
        let raw: RawManagerConfig = toml_edit::de::from_str(manager_source)?;
        require_schema(raw.schema_version)?;
        if raw.max_starting == 0 || raw.max_starting > MAX_STARTING_HARD_LIMIT {
            return Err(ConfigError::InvalidField {
                field: "max_starting",
                reason: format!("must be between 1 and {MAX_STARTING_HARD_LIMIT}"),
            });
        }

        let default_group = ServiceId::new(raw.default_group)?;
        let mut groups = BTreeMap::new();
        for (name, group) in raw.groups {
            let id = ServiceId::new(name)?;
            let dependencies = Dependencies::from_lists(
                &id,
                group.requires,
                group.wants,
                group.after,
                group.conflicts,
            )?;
            groups.insert(id.clone(), GroupDefinition { id, dependencies });
        }
        if !groups.contains_key(&default_group) {
            return Err(ConfigError::MissingDefaultGroup(default_group));
        }

        let mut services = BTreeMap::new();
        for (id, source) in service_sources {
            let definition = ServiceDefinition::parse(id.clone(), source, scope)?;
            if services.insert(id.clone(), definition).is_some() {
                return Err(ConfigError::DuplicateDefinition(id));
            }
        }
        if let Some(id) = services.keys().find(|id| groups.contains_key(*id)) {
            return Err(ConfigError::AmbiguousDefinition(id.clone()));
        }

        let warnings = validate_references(&services, &groups)?;
        validate_ordering(&services, &groups)?;
        validate_conflicts(&services, &groups)?;

        Ok(Self {
            services,
            groups,
            default_group,
            max_starting: raw.max_starting,
            warnings,
        })
    }

    #[must_use]
    pub fn services(&self) -> &BTreeMap<ServiceId, ServiceDefinition> {
        &self.services
    }

    #[must_use]
    pub fn groups(&self) -> &BTreeMap<ServiceId, GroupDefinition> {
        &self.groups
    }

    #[must_use]
    pub const fn default_group(&self) -> &ServiceId {
        &self.default_group
    }

    #[must_use]
    pub const fn max_starting(&self) -> usize {
        self.max_starting
    }

    #[must_use]
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.warnings
    }

    #[must_use]
    pub fn dependencies(&self, id: &ServiceId) -> Option<&Dependencies> {
        self.services
            .get(id)
            .map(|service| &service.dependencies)
            .or_else(|| self.groups.get(id).map(|group| &group.dependencies))
    }

    #[must_use]
    pub fn activation_services(&self, root: &ServiceId) -> Option<BTreeSet<ServiceId>> {
        if !self.contains_node(root) {
            return None;
        }
        let mut services = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = VecDeque::from([root.clone()]);
        while let Some(node) = pending.pop_front() {
            if !visited.insert(node.clone()) {
                continue;
            }
            if self.services.contains_key(&node) {
                services.insert(node.clone());
            }
            if let Some(dependencies) = self.dependencies(&node) {
                pending.extend(dependencies.requires.iter().cloned());
                pending.extend(
                    dependencies
                        .wants
                        .iter()
                        .filter(|wanted| self.contains_node(wanted))
                        .cloned(),
                );
            }
        }
        Some(services)
    }

    fn contains_node(&self, id: &ServiceId) -> bool {
        self.services.contains_key(id) || self.groups.contains_key(id)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManagerConfig {
    schema_version: u32,
    default_group: String,
    #[serde(default = "default_max_starting")]
    max_starting: usize,
    groups: BTreeMap<String, RawGroup>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    wants: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
}

const fn default_max_starting() -> usize {
    MAX_STARTING_HARD_LIMIT
}

fn validate_references(
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    groups: &BTreeMap<ServiceId, GroupDefinition>,
) -> Result<Vec<ConfigWarning>, ConfigError> {
    let contains = |id: &ServiceId| services.contains_key(id) || groups.contains_key(id);
    let mut warnings = Vec::new();
    for (owner, dependencies) in all_dependencies(services, groups) {
        for (relation, targets) in [
            ("required", &dependencies.requires),
            ("ordering", &dependencies.after),
            ("conflict", &dependencies.conflicts),
        ] {
            if let Some(target) = targets.iter().find(|target| !contains(target)) {
                return Err(ConfigError::MissingDependency {
                    owner: owner.clone(),
                    relation,
                    target: target.clone(),
                });
            }
        }
        warnings.extend(
            dependencies
                .wants
                .iter()
                .filter(|target| !contains(target))
                .cloned()
                .map(|target| ConfigWarning::MissingWanted {
                    owner: owner.clone(),
                    target,
                }),
        );
    }
    Ok(warnings)
}

fn validate_ordering(
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    groups: &BTreeMap<ServiceId, GroupDefinition>,
) -> Result<(), ConfigError> {
    let mut states = HashMap::<ServiceId, VisitState>::new();
    let mut stack = Vec::new();
    for node in services.keys().chain(groups.keys()) {
        if !states.contains_key(node) {
            visit(node, services, groups, &mut states, &mut stack)?;
        }
    }
    Ok(())
}

fn visit(
    node: &ServiceId,
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    groups: &BTreeMap<ServiceId, GroupDefinition>,
    states: &mut HashMap<ServiceId, VisitState>,
    stack: &mut Vec<ServiceId>,
) -> Result<(), ConfigError> {
    states.insert(node.clone(), VisitState::Visiting);
    stack.push(node.clone());

    let dependencies = node_dependencies(node, services, groups).expect("validated graph node");
    for target in dependencies.requires.iter().chain(&dependencies.after) {
        match states.get(target) {
            Some(VisitState::Visiting) => {
                let start = stack
                    .iter()
                    .position(|entry| entry == target)
                    .expect("visiting node is on DFS stack");
                let mut cycle = stack[start..].to_vec();
                cycle.push(target.clone());
                return Err(ConfigError::DependencyCycle(cycle));
            }
            Some(VisitState::Visited) => {}
            None => visit(target, services, groups, states, stack)?,
        }
    }

    let popped = stack.pop();
    debug_assert_eq!(popped.as_ref(), Some(node));
    states.insert(node.clone(), VisitState::Visited);
    Ok(())
}

fn validate_conflicts(
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    groups: &BTreeMap<ServiceId, GroupDefinition>,
) -> Result<(), ConfigError> {
    for group in groups.keys() {
        let active = activation_nodes(group, services, groups);
        for node in &active {
            let dependencies =
                node_dependencies(node, services, groups).expect("known active node");
            if let Some(other) = dependencies
                .conflicts
                .iter()
                .find(|other| active.contains(*other))
            {
                return Err(ConfigError::EnabledConflict {
                    group: group.clone(),
                    left: node.clone(),
                    right: other.clone(),
                });
            }
        }
    }
    Ok(())
}

fn activation_nodes(
    root: &ServiceId,
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    groups: &BTreeMap<ServiceId, GroupDefinition>,
) -> BTreeSet<ServiceId> {
    let mut active = BTreeSet::new();
    let mut pending = VecDeque::from([root.clone()]);
    while let Some(node) = pending.pop_front() {
        if !active.insert(node.clone()) {
            continue;
        }
        let dependencies = node_dependencies(&node, services, groups).expect("known active node");
        pending.extend(dependencies.requires.iter().cloned());
        pending.extend(
            dependencies
                .wants
                .iter()
                .filter(|wanted| services.contains_key(*wanted) || groups.contains_key(*wanted))
                .cloned(),
        );
    }
    active
}

fn all_dependencies<'a>(
    services: &'a BTreeMap<ServiceId, ServiceDefinition>,
    groups: &'a BTreeMap<ServiceId, GroupDefinition>,
) -> impl Iterator<Item = (&'a ServiceId, &'a Dependencies)> {
    services
        .iter()
        .map(|(id, service)| (id, &service.dependencies))
        .chain(groups.iter().map(|(id, group)| (id, &group.dependencies)))
}

fn node_dependencies<'a>(
    node: &ServiceId,
    services: &'a BTreeMap<ServiceId, ServiceDefinition>,
    groups: &'a BTreeMap<ServiceId, GroupDefinition>,
) -> Option<&'a Dependencies> {
    services
        .get(node)
        .map(|service| &service.dependencies)
        .or_else(|| groups.get(node).map(|group| &group.dependencies))
}

#[derive(Clone, Copy)]
enum VisitState {
    Visiting,
    Visited,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dependencies: &str) -> String {
        format!(
            r#"
schema_version = 1
[process]
command = ["/usr/bin/true"]
{dependencies}
"#
        )
    }

    fn manager(groups: &str) -> String {
        format!(
            r#"
schema_version = 1
default_group = "boot"
{groups}
"#
        )
    }

    #[test]
    fn builds_snapshot_and_activation_closure() {
        let manager = manager(
            r#"
[groups.boot]
wants = ["web"]
"#,
        );
        let web = service(
            r#"
[dependencies]
requires = ["db"]
"#,
        );
        let db = service("");
        let snapshot = ConfigSnapshot::build(
            &manager,
            [
                (ServiceId::new("web").unwrap(), web.as_str()),
                (ServiceId::new("db").unwrap(), db.as_str()),
            ],
            ManagerScope::System,
        )
        .unwrap();

        assert_eq!(snapshot.services().len(), 2);
        assert_eq!(snapshot.max_starting(), 4096);
        assert_eq!(
            snapshot
                .activation_services(snapshot.default_group())
                .unwrap(),
            BTreeSet::from([
                ServiceId::new("db").unwrap(),
                ServiceId::new("web").unwrap(),
            ])
        );
    }

    #[test]
    fn rejects_missing_strict_target_but_warns_for_missing_want() {
        let manager_source = manager(
            r#"
[groups.boot]
wants = ["optional"]
"#,
        );
        let snapshot = ConfigSnapshot::build(&manager_source, [], ManagerScope::System).unwrap();
        assert_eq!(snapshot.warnings().len(), 1);

        let broken = service(
            r#"
[dependencies]
requires = ["missing"]
"#,
        );
        assert!(matches!(
            ConfigSnapshot::build(
                &manager("[groups.boot]"),
                [(ServiceId::new("broken").unwrap(), broken.as_str())],
                ManagerScope::System,
            ),
            Err(ConfigError::MissingDependency { .. })
        ));
    }

    #[test]
    fn reports_ordering_cycle() {
        let manager = manager(
            r#"
[groups.boot]
wants = ["a"]
"#,
        );
        let a = service("[dependencies]\nrequires = [\"b\"]");
        let b = service("[dependencies]\nafter = [\"a\"]");
        let error = ConfigSnapshot::build(
            &manager,
            [
                (ServiceId::new("a").unwrap(), a.as_str()),
                (ServiceId::new("b").unwrap(), b.as_str()),
            ],
            ManagerScope::System,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::DependencyCycle(_)));
        assert!(error.to_string().contains("a -> b -> a"));
    }

    #[test]
    fn rejects_conflicts_in_activation_closure() {
        let manager = manager(
            r#"
[groups.boot]
wants = ["a", "b"]
"#,
        );
        let a = service("[dependencies]\nconflicts = [\"b\"]");
        let b = service("");

        assert!(matches!(
            ConfigSnapshot::build(
                &manager,
                [
                    (ServiceId::new("a").unwrap(), a.as_str()),
                    (ServiceId::new("b").unwrap(), b.as_str()),
                ],
                ManagerScope::System,
            ),
            Err(ConfigError::EnabledConflict { .. })
        ));
    }

    #[test]
    fn rejects_unknown_manager_field_and_excessive_parallelism() {
        let unknown = r#"
schema_version = 1
default_group = "boot"
unknown = true
[groups.boot]
"#;
        let excessive = r#"
schema_version = 1
default_group = "boot"
max_starting = 4097
[groups.boot]
"#;

        for source in [unknown, excessive] {
            assert!(ConfigSnapshot::build(source, [], ManagerScope::System).is_err());
        }
    }

    #[test]
    fn rejects_service_group_name_collision() {
        let manager = manager("[groups.boot]\n[groups.same]");
        let same = service("");

        assert!(matches!(
            ConfigSnapshot::build(
                &manager,
                [(ServiceId::new("same").unwrap(), same.as_str())],
                ManagerScope::System,
            ),
            Err(ConfigError::AmbiguousDefinition(_))
        ));
    }
}
