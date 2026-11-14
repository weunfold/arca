//! `route_add`, `route_remove` and `route_list`: the names a person types to reach an instance of this node, served by
//! the node's edge ([`crate::route`]).
//!
//! A route is kept in the desired-state record of the edge, so an add or a remove is a change of that record like any
//! other: the reconciler writes the edge's NIC rules from it (each route's target opened on its port). Then the kernel
//! renders the whole Caddyfile from the record, writes it into the edge through the Incus files API, and, when the edge
//! runs, reloads Caddy in it. A reload Caddy refuses leaves the old config serving; the route stays recorded and the
//! answer says the reload failed, so the next change, or the same add again, writes it once more.
//!
//! Both writes take a doctor token, or run from a proposal the owner approved (`action_propose`), as `service_connect`
//! does. `route_list` is a read.
use super::*;
use crate::route::{self, EdgeRecord, Reach, Route};
use crate::service::ServiceConfig;
use arca_incus::exec::{ExecCommand, ServiceFile};
use arca_kernel::desired::Entry;

/// The fields each route verb takes.
pub(super) fn allowed(verb: &str) -> &'static [&'static str] {
    match verb {
        "route_add" => &["hostname", "instance", "project", "port", "reach"],
        "route_remove" => &["hostname"],
        _ => &[],
    }
}

fn hostname_arg(args: &Value) -> Result<String, CallError> {
    args["hostname"]
        .as_str()
        .map(str::to_ascii_lowercase)
        .filter(|name| route::hostname_ok(name))
        .ok_or_else(|| {
            CallError::Invalid(
                "hostname: expected a DNS name such as app.example.com (letters, digits and -, at least two labels, no wildcard)"
                    .into(),
            )
        })
}

/// The node's edge: the one service of the node's own project whose record carries routes, as its template says.
fn edge_of(entries: &[Entry]) -> Option<(Entry, ServiceConfig)> {
    service_entries(entries)
        .into_iter()
        .filter(|(_, cfg)| cfg.edge.is_some() && cfg.tenant.is_none() && !cfg.adopted)
        .min_by(|a, b| a.1.name.cmp(&b.1.name))
}

/// What a refusal says, in words, for an answer's note.
fn words(error: &CallError) -> String {
    match error {
        CallError::Refused { message, .. }
        | CallError::UntrustedRefused { message, .. }
        | CallError::RefusedWith { message, .. } => message.clone(),
        CallError::Invalid(message) => message.clone(),
        _ => "this node could not reach the edge".into(),
    }
}

fn no_edge() -> CallError {
    service_refused(
        "this node has no edge yet: create one from the arca-edge template (service_create), set its CLOUDFLARE_API_TOKEN secret and start it, then add the route",
    )
}

/// Where a person reaches the edge: its host port's address, or its own address when it has none.
fn edge_address(cfg: &ServiceConfig) -> String {
    match (&cfg.private_port, &cfg.lan) {
        (Some(private), _) => format!("{}:{}", private.address, private.port),
        (None, Some(lan)) => format!("every address of this node, port {}", lan.port),
        (None, None) => cfg.ipv4.clone(),
    }
}

/// What a route's approval card says of the edge, as the node holds it when the proposal is made.
pub(super) fn edge_fact(entries: &[Entry]) -> String {
    match edge_of(entries) {
        Some((_, edge)) => format!("the service {}, reached at {}", edge.name, edge_address(&edge)),
        None => "none yet: this node has no edge, so the route cannot be served until one is created".into(),
    }
}

impl ServiceHandler {
    pub(super) fn call_routes(
        &self,
        args: &Value,
        caller: &Caller,
        entries: &[Entry],
        now: u64,
    ) -> Result<Value, CallError> {
        if self.verb == "route_list" {
            return Ok(self.route_list(entries));
        }
        let hostname = hostname_arg(args)?;
        let (edge_entry, mut edge) = edge_of(entries).ok_or_else(no_edge)?;
        let mut record = edge.edge.clone().unwrap_or_default();
        let (answer, removed) = if self.verb == "route_add" {
            let route = self.route_from(args, &hostname, &edge, entries)?;
            let answer = route.view();
            record.put(route);
            (answer, None)
        } else {
            let removed = record.remove(&hostname).ok_or_else(|| {
                service_not_found(format!("route {hostname}: this node has no route of that name"))
            })?;
            (removed.view(), Some(removed))
        };
        edge.edge = Some(record.clone());
        edge.environment_patch = Some(BTreeSet::new());
        self.put(std::slice::from_ref(&edge), now)?;
        let applied = self.write_edge(&edge_entry, &edge, &record, caller);
        let mut answer = answer;
        answer["edge"] = json!({
            "name": edge.name,
            "address": edge_address(&edge),
        });
        match applied {
            Ok(reloaded) => {
                answer["applied"] = json!(true);
                answer["reloaded"] = json!(reloaded);
                if !reloaded {
                    answer["note"] = json!(
                        "the edge is not running: its new config is written and Caddy reads it when the edge starts"
                    );
                }
            }
            Err(why) => {
                answer["applied"] = json!(false);
                answer["reloaded"] = json!(false);
                answer["note"] = json!(format!(
                    "the route is recorded, but the edge did not take its new config: {why}. Add the route again once the edge runs"
                ));
            }
        }
        if let Some(removed) = removed {
            answer["removed"] = json!(true);
            answer["hostname"] = json!(removed.hostname);
        }
        Ok(answer)
    }

    /// The route an add asks for, with the target's address as Incus has it now.
    fn route_from(
        &self,
        args: &Value,
        hostname: &str,
        edge: &ServiceConfig,
        entries: &[Entry],
    ) -> Result<Route, CallError> {
        let instance = args["instance"]
            .as_str()
            .filter(|name| arca_incus::exec::node_ok(name))
            .ok_or_else(|| CallError::Invalid("instance: expected an instance name".into()))?
            .to_string();
        let project = match args.get("project") {
            None => arca_netproject::PROJECT.to_string(),
            Some(value) => value
                .as_str()
                .filter(|name| route::project_ok(name))
                .ok_or_else(|| CallError::Invalid("project: expected an Incus project name".into()))?
                .to_string(),
        };
        let port = args["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| CallError::Invalid("port: expected a tcp port from 1 to 65535".into()))?;
        let reach = match args.get("reach") {
            None => Reach::Private,
            Some(value) => value.as_str().and_then(Reach::parse).ok_or_else(|| {
                CallError::Invalid("reach: expected \"private\" or \"public\"".into())
            })?,
        };
        if instance == edge.instance && project == edge.project {
            return Err(service_refused("a route cannot point at the edge itself"));
        }
        let read = UnixIncus::new(self.cfg.incus.socket.clone());
        let full = read
            .get(&format!(
                "/1.0/instances/{instance}?project={project}&recursion=1"
            ))
            .map_err(|_| {
                service_not_found(format!(
                    "instance {instance} in project {project}: this node's Incus has no such instance, or did not answer. Nothing was changed"
                ))
            })?;
        let address = route::instance_address(&full).ok_or_else(|| {
            service_refused(format!(
                "instance {instance} has no IPv4 address this node can read: give its NIC a fixed ipv4.address, or start it, then add the route again. Nothing was changed"
            ))
        })?;
        // An Arca service of this node is reached through a link from the edge, which opens its first port.
        if project == arca_netproject::PROJECT
            && let Some((_, target)) = service_entries(entries)
                .into_iter()
                .find(|(_, cfg)| cfg.tenant.is_none() && cfg.instance == instance)
        {
            let linked = edge.links.iter().find(|link| link.to == target.name);
            match linked {
                Some(link) if link.port == port => {}
                Some(link) => {
                    return Err(service_refused(format!(
                        "the edge is linked to the service {} on port {}, not {port}: a route to an Arca service uses the port its link opens",
                        target.name, link.port
                    )));
                }
                None => {
                    return Err(service_refused(format!(
                        "the edge is not linked to the service {}: connect them first (service_connect from {} to {}), then add the route",
                        target.name, edge.name, target.name
                    )));
                }
            }
        }
        Ok(Route {
            hostname: hostname.to_string(),
            instance,
            project,
            address,
            port,
            reach,
        })
    }

    /// Writes the Caddyfile of `record` into the edge, and reloads Caddy when the edge runs. `Ok(false)` when it does not
    /// run: the file is there for its next start.
    fn write_edge(
        &self,
        entry: &Entry,
        edge: &ServiceConfig,
        record: &EdgeRecord,
        caller: &Caller,
    ) -> Result<bool, String> {
        let owned = self
            .live_owned(entry, edge)
            .map_err(|error| words(&error))?;
        let target = self
            .exec_target(entry, edge)
            .map_err(|error| words(&error))?;
        let writer = self
            .exec
            .writer(&target, Duration::from_secs(10))
            .map_err(|_| "this node could not reach Incus".to_string())?;
        writer
            .write_service_file(
                &owned,
                &ServiceFile::EdgeConfig,
                route::caddyfile(record).as_bytes(),
            )
            .map_err(|_| "Incus did not take the edge's config file".to_string())?;
        let (state, _) = writer
            .get_etag(owned.target())
            .map_err(|_| "this node could not read the edge's state".to_string())?;
        if state["status"] != "Running" {
            return Ok(false);
        }
        let secrets = service_secret_values(&self.cfg.state_dir.join("secrets"));
        self.exec
            .run(
                &target,
                &caller.token_id,
                self.verb,
                &ExecCommand::EdgeReload,
                &secrets,
                None,
            )
            .map(|_| true)
            .map_err(|error| match error {
                crate::service_exec::ExecError::Failed(why) => {
                    format!("Caddy refused the reload ({why}); the old config still serves")
                }
                other => words(&other.call_error()),
            })
    }

    fn route_list(&self, entries: &[Entry]) -> Value {
        let Some((_, edge)) = edge_of(entries) else {
            return json!({ "edge": null, "routes": [] });
        };
        let routes = edge
            .edge
            .as_ref()
            .map(|record| record.routes.iter().map(Route::view).collect::<Vec<_>>())
            .unwrap_or_default();
        json!({
            "edge": {
                "name": edge.name,
                "address": edge_address(&edge),
                "ipv4": edge.ipv4,
            },
            "routes": routes,
        })
    }
}
