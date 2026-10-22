//! Nodes, principals and permissions (ADR-0003): who may do what through which node.
//!
//! Built once from the configuration. Protocol addons say through which node and on whose behalf
//! they call (a [`CallContext`]); [`Directory::require`] resolves the caller and checks the
//! permission, and the node's backend tells the caller which partition to work in.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use anyhow::bail;
use medicus_core::event::Origin;
use medicus_core::{
    AeTitle, CallContext, Error, ErrorKind, NodeInfo, Partition, Permission, Result,
};

use crate::config::{Config, GrantConfig, NodeConfig};

/// The node of a zero-configuration archive.
pub(crate) const DEFAULT_NODE: &str = "MEDICUS";

/// What stands behind a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Backend {
    Local(Partition),
    /// A proxy for the named peer.
    Remote(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) backend: Backend,
    pub(crate) ae_title: Option<AeTitle>,
    pub(crate) dicomweb_path: Option<String>,
}

impl Node {
    pub(crate) fn info(&self) -> NodeInfo {
        NodeInfo {
            name: self.name.clone(),
            ae_title: self.ae_title.clone(),
            dicomweb_path: self.dicomweb_path.clone(),
            remote: matches!(self.backend, Backend::Remote(_)),
        }
    }

    /// The local partition, or `Unsupported` for a remote node.
    pub(crate) fn partition(&self) -> Result<&Partition> {
        match &self.backend {
            Backend::Local(partition) => Ok(partition),
            Backend::Remote(peer) => Err(Error::unsupported(format!(
                "node {} is a proxy for peer {peer:?}; this operation needs a local node",
                self.name
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct Principal {
    name: String,
    dimse: Option<(AeTitle, Vec<ipnet::IpNet>)>,
    roles: Vec<String>,
}

/// Who a caller is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller<'a> {
    /// The core itself (imports, internal components, verified job executors).
    System,
    Principal(&'a str),
    Anonymous,
}

impl std::fmt::Display for Caller<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::Principal(name) => write!(f, "principal {name:?}"),
            Self::Anonymous => f.write_str("anonymous caller"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Directory {
    nodes: BTreeMap<String, Node>,
    principals: Vec<Principal>,
    /// Role → node (or `*`) → permissions.
    roles: BTreeMap<String, Vec<GrantConfig>>,
    anonymous_roles: Vec<String>,
    /// No access control configured: everyone may do everything.
    open: bool,
}

impl Directory {
    pub(crate) fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let nodes = nodes_from_config(cfg)?;

        for (role, config) in &cfg.roles {
            for grant in &config.grants {
                for node in &grant.nodes {
                    if node != "*" && !nodes.contains_key(node) {
                        bail!("role {role:?} grants on unknown node {node:?}");
                    }
                }
            }
        }
        let known_role = |role: &String, whose: &str| -> anyhow::Result<()> {
            if cfg.roles.contains_key(role) {
                Ok(())
            } else {
                bail!("{whose} has unknown role {role:?}")
            }
        };
        let mut principals = Vec::new();
        for (name, p) in &cfg.principals {
            for role in &p.roles {
                known_role(role, &format!("principal {name:?}"))?;
            }
            principals.push(Principal {
                name: name.clone(),
                dimse: p
                    .dimse
                    .as_ref()
                    .map(|d| (d.calling_ae.clone(), d.networks.clone())),
                roles: p.roles.clone(),
            });
        }
        for role in &cfg.access.anonymous_roles {
            known_role(role, "access.anonymous_roles")?;
        }
        let open = cfg.roles.is_empty()
            && cfg.principals.is_empty()
            && cfg.access.anonymous_roles.is_empty();
        if open {
            tracing::warn!(
                "no roles or principals configured: access control is off, every caller may do everything"
            );
        }
        Ok(Self {
            nodes,
            principals,
            roles: cfg
                .roles
                .iter()
                .map(|(name, r)| (name.clone(), r.grants.clone()))
                .collect(),
            anonymous_roles: cfg.access.anonymous_roles.clone(),
            open,
        })
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub(crate) fn node(&self, name: &str) -> Result<&Node> {
        self.nodes
            .get(name)
            .ok_or_else(|| Error::not_found(format!("node {name:?} is not configured")))
    }

    /// Who `origin` is. Core-internal origins (imports, internal components) are only trusted
    /// from the core itself, so the caller says whether `origin` came from an addon; job origins
    /// from addons are verified by the caller before (see `services`).
    pub(crate) fn caller<'a>(&'a self, origin: &Origin, from_addon: bool) -> Caller<'a> {
        match origin {
            Origin::Internal { .. } | Origin::Import { .. } | Origin::Job { .. } if !from_addon => {
                Caller::System
            }
            Origin::Dimse {
                calling_ae, peer, ..
            } => {
                let ip = peer.parse::<SocketAddr>().ok().map(|a| a.ip());
                self.principals
                    .iter()
                    .find(|p| {
                        p.dimse.as_ref().is_some_and(|(ae, networks)| {
                            ae == calling_ae
                                && (networks.is_empty()
                                    || ip
                                        .is_some_and(|ip| networks.iter().any(|n| n.contains(&ip))))
                        })
                    })
                    .map_or(Caller::Anonymous, |p| Caller::Principal(&p.name))
            }
            Origin::Dicomweb {
                principal: Some(name),
                ..
            }
            | Origin::AdminApi {
                principal: Some(name),
                ..
            } => self
                .principals
                .iter()
                .find(|p| &p.name == name)
                .map_or(Caller::Anonymous, |p| Caller::Principal(&p.name)),
            _ => Caller::Anonymous,
        }
    }

    /// What `caller` may do on `node`.
    pub(crate) fn permissions(&self, caller: &Caller<'_>, node: &str) -> BTreeSet<Permission> {
        if self.open || *caller == Caller::System {
            return Permission::ALL.into_iter().collect();
        }
        let roles: &[String] = match caller {
            Caller::Principal(name) => self
                .principals
                .iter()
                .find(|p| p.name == *name)
                .map_or(&[], |p| p.roles.as_slice()),
            Caller::Anonymous | Caller::System => &self.anonymous_roles,
        };
        roles
            .iter()
            .filter_map(|role| self.roles.get(role))
            .flatten()
            .filter(|grant| grant.nodes.iter().any(|n| n == "*" || n == node))
            .flat_map(|grant| grant.permissions.iter().copied())
            .collect()
    }

    /// The context's node, if its caller has `permission` on it; `PermissionDenied` otherwise.
    pub(crate) fn require(
        &self,
        context: &CallContext,
        caller: &Caller<'_>,
        permission: Permission,
    ) -> Result<&Node> {
        let node = self.node(&context.node)?;
        if self.permissions(caller, &node.name).contains(&permission) {
            Ok(node)
        } else {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{caller} may not {} on node {}",
                    permission_verb(permission),
                    node.name
                ),
            ))
        }
    }
}

/// The configured nodes, or the zero-configuration node `MEDICUS`.
fn nodes_from_config(cfg: &Config) -> anyhow::Result<BTreeMap<String, Node>> {
    let mut partitions: BTreeSet<&Partition> = cfg.partitions.keys().collect();
    let default = Partition::default_partition();
    partitions.insert(&default);

    let mut configured = cfg.nodes.clone();
    if configured.is_empty() {
        configured.insert(
            DEFAULT_NODE.to_owned(),
            NodeConfig {
                partition: Some(Partition::default_partition()),
                peer: None,
                dimse: Some(crate::config::NodeDimseConfig {
                    ae_title: AeTitle::parse(DEFAULT_NODE)?,
                }),
                dicomweb: Some(crate::config::NodeDicomwebConfig {
                    path: DEFAULT_NODE.to_lowercase(),
                }),
            },
        );
    }
    let mut nodes = BTreeMap::new();
    let mut ae_titles = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for (name, node) in configured {
        let backend = match (&node.partition, &node.peer) {
            (Some(p), None) => {
                if !partitions.contains(p) {
                    bail!("node {name:?}: partition {p} is not configured");
                }
                Backend::Local(p.clone())
            }
            (None, Some(peer)) => {
                if !cfg.peers.contains_key(peer) {
                    bail!("node {name:?}: peer {peer:?} is not configured");
                }
                Backend::Remote(peer.clone())
            }
            _ => bail!("node {name:?} needs exactly one of `partition` and `peer`"),
        };
        let ae_title = node.dimse.map(|d| d.ae_title);
        if let Some(ae) = &ae_title
            && !ae_titles.insert(ae.clone())
        {
            bail!("AE title {ae} is used by two nodes");
        }
        let dicomweb_path = node.dicomweb.map(|d| d.path);
        if let Some(path) = &dicomweb_path {
            let valid = !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid || !paths.insert(path.clone()) {
                bail!("node {name:?}: DICOMweb path {path:?} is invalid or used twice");
            }
        }
        nodes.insert(
            name.clone(),
            Node {
                name,
                backend,
                ae_title,
                dicomweb_path,
            },
        );
    }

    Ok(nodes)
}

fn permission_verb(permission: Permission) -> &'static str {
    match permission {
        Permission::Query => "query",
        Permission::Retrieve => "retrieve",
        Permission::Store => "store",
        Permission::Send => "send",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        use figment::providers::{Format, Serialized, Toml};
        figment::Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .unwrap()
    }

    fn dimse(calling: &str, peer: &str) -> Origin {
        Origin::Dimse {
            calling_ae: AeTitle::parse(calling).unwrap(),
            called_ae: AeTitle::parse("RAD").unwrap(),
            peer: peer.into(),
            association: None,
        }
    }

    const CONFIG: &str = r#"
        [partitions.tenant-a]
        [nodes.RAD]
        partition = "tenant-a"
        dimse = { ae_title = "RAD" }
        [nodes.VIEW]
        partition = "tenant-a"
        dicomweb = { path = "view" }
        [roles.modality]
        grants = [{ nodes = ["RAD"], permissions = ["store"] }]
        [roles.viewer]
        grants = [{ nodes = ["*"], permissions = ["query", "retrieve"] }]
        [principals.ct01]
        dimse = { calling_ae = "CT01", networks = ["10.0.0.0/24"] }
        roles = ["modality"]
        [principals.alice]
        roles = ["viewer"]
    "#;

    #[test]
    fn principals_get_the_permissions_of_their_roles() {
        let dir = Directory::from_config(&config(CONFIG)).unwrap();
        let ct = dir.caller(&dimse("CT01", "10.0.0.7:4000"), true);
        assert_eq!(ct, Caller::Principal("ct01"));
        assert_eq!(dir.permissions(&ct, "RAD"), [Permission::Store].into());
        assert!(dir.permissions(&ct, "VIEW").is_empty());
        // The right AE title from the wrong network is somebody else.
        assert_eq!(
            dir.caller(&dimse("CT01", "192.168.1.5:4000"), true),
            Caller::Anonymous
        );

        let alice = dir.caller(
            &Origin::Dicomweb {
                remote_addr: "1.2.3.4:5".into(),
                principal: Some("alice".into()),
                request_id: None,
            },
            true,
        );
        assert_eq!(
            dir.permissions(&alice, "VIEW"),
            [Permission::Query, Permission::Retrieve].into()
        );
        let context = CallContext::new("RAD", dimse("CT01", "10.0.0.7:1"));
        let err = dir
            .require(&context, &alice, Permission::Store)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::PermissionDenied);
        assert!(dir.require(&context, &ct, Permission::Store).is_ok());
        assert_eq!(
            dir.node("RAD").unwrap().partition().unwrap().as_str(),
            "tenant-a"
        );
    }

    #[test]
    fn addons_cannot_claim_core_origins() {
        let dir = Directory::from_config(&config(CONFIG)).unwrap();
        let import = Origin::Import { path: "/x".into() };
        assert_eq!(dir.caller(&import, false), Caller::System);
        assert_eq!(dir.caller(&import, true), Caller::Anonymous);
        assert!(dir.permissions(&Caller::Anonymous, "RAD").is_empty());
    }

    #[test]
    fn zero_config_is_one_open_node() {
        let dir = Directory::from_config(&Config::default()).unwrap();
        let node = dir.node(DEFAULT_NODE).unwrap();
        assert_eq!(node.backend, Backend::Local(Partition::default_partition()));
        assert_eq!(node.ae_title.as_ref().unwrap().as_str(), "MEDICUS");
        assert_eq!(dir.permissions(&Caller::Anonymous, DEFAULT_NODE).len(), 4);
    }

    #[test]
    fn invalid_configurations_are_refused() {
        for bad in [
            "[nodes.X]\npartition = \"nope\"",
            "[nodes.X]\npeer = \"nope\"",
            "[nodes.X]\n",
            "[nodes.A]\npartition = \"default\"\ndimse = { ae_title = \"SAME\" }\n[nodes.B]\npartition = \"default\"\ndimse = { ae_title = \"SAME\" }",
            "[roles.r]\ngrants = [{ nodes = [\"NOPE\"], permissions = [\"query\"] }]",
            "[principals.p]\nroles = [\"missing\"]",
        ] {
            assert!(Directory::from_config(&config(bad)).is_err(), "{bad}");
        }
    }
}
