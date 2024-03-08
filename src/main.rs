#![doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))]

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use clap::Parser;
use client::{KanidmClient, ENDPOINT_GROUP, ENDPOINT_OAUTH2, ENDPOINT_PERSON};
use color_eyre::{
    eyre::{bail, eyre, Result},
    owo_colors::OwoColorize,
    Section,
};

use serde_json::{json, Value};
use state::State;

mod client;
mod state;

const PROVISION_TRACKING_GROUP: &str = "ext_idm_provisioned_entities";

fn log_status(message: &str) {
    println!("{}", message.blue().bold());
}

fn log_event(event: &str, message: &str) {
    println!("{:>12} {}", event.green().bold(), message);
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The URL of the kanidm instance
    #[arg(long)]
    url: String,

    /// A JSON file describing the desired target state. Refer to the README for a description of
    /// the required schema.
    #[arg(long)]
    state: PathBuf,

    /// DANGEROUS! Accept invalid TLS certificates, e.g. for testing instances.
    #[arg(long)]
    accept_invalid_certs: bool,

    /// Automatically remove orphaned entities that were provisioned previously but have since been removed
    /// from the state file. This works by assigning all provisioned entities to a common group and
    /// deleting any entities in that group that are not found in the state file.
    #[arg(long)]
    auto_remove: bool,
}

/// Return a map of all tracked entities and ensure that their names are unique.
fn all_tracked_entities(state: &State) -> Result<Vec<String>> {
    let mut entity_names: HashMap<_, Vec<&str>> = HashMap::new();
    for i in state.groups.keys() {
        entity_names.entry(i.to_owned()).or_default().push("group");
    }
    for i in state.persons.keys() {
        entity_names.entry(i.to_owned()).or_default().push("person");
    }
    for i in state.systems.oauth2.keys() {
        entity_names.entry(i.to_owned()).or_default().push("oauth2");
    }

    let mut error = eyre!("One or more entities have the same name (see notes)");
    let mut any_bad = false;
    for (k, v) in &entity_names {
        if v.len() > 1 {
            error = error.note(format!("{k} is used multiple times as {v:?}"));
            any_bad = true;
        }
    }

    if any_bad {
        return Err(error);
    }

    Ok(entity_names.keys().cloned().collect())
}

macro_rules! update_attrs {
    ($kanidm_client:expr, $endpoint:expr, $existing:expr, $name:expr, [ $( $key:literal : $value:expr ),*, ]) => {
        $(
            $kanidm_client.update_entity_attrs($endpoint, $existing, $name, $key, $value, false)?;
        )*
    };
}

macro_rules! update_oauth2 {
    ($kanidm_client:expr, $existing:expr, $name:expr, [ $( $key:literal : $value:expr ),*, ]) => {
        $(
            if let Some(value) = $value {
                $kanidm_client.update_oauth2_attrs($existing, $name, $key, vec![value])?;
            } else {
                $kanidm_client.update_oauth2_attrs($existing, $name, $key, vec![])?;
            }
        )*
    };
}

fn sync_groups(
    state: &State,
    kanidm_client: &KanidmClient,
    existing_groups: &mut HashMap<String, Value>,
    preexisting_entity_names: &HashSet<String>,
) -> Result<()> {
    log_status("Syncing groups");
    for (name, group) in &state.groups {
        if group.present {
            if !existing_groups.contains_key(name) {
                if preexisting_entity_names.contains(name) {
                    bail!("Cannot create group '{name}' because the name is already in use by another entity!");
                }

                kanidm_client.create_entity(ENDPOINT_GROUP, name, &json!({ "attrs": { "name": [ name ] } }))?;
                existing_groups.clear();
                existing_groups.extend(kanidm_client.get_entities(ENDPOINT_GROUP)?);
            }
        } else if existing_groups.contains_key(name) {
            kanidm_client.delete_entity(ENDPOINT_GROUP, name)?;
        }
    }

    Ok(())
}

fn sync_persons(
    state: &State,
    kanidm_client: &KanidmClient,
    existing_persons: &mut HashMap<String, Value>,
    preexisting_entity_names: &HashSet<String>,
) -> Result<()> {
    log_status("Syncing persons");
    for (name, person) in &state.persons {
        if person.present {
            if !existing_persons.contains_key(name) {
                if preexisting_entity_names.contains(name) {
                    bail!("Cannot create person '{name}' because the name is already in use by another entity!");
                }

                kanidm_client.create_entity(
                    ENDPOINT_PERSON,
                    name,
                    &json!({ "attrs": {
                        "name": [ name ],
                        "displayname": [ person.display_name ]
                    }}),
                )?;
                existing_persons.clear();
                existing_persons.extend(kanidm_client.get_entities(ENDPOINT_PERSON)?);
            }

            update_attrs!(kanidm_client, ENDPOINT_PERSON, &existing_persons, &name, [
                "displayname": vec![person.display_name.clone()],
                "legalname": vec![person.legal_name.clone()],
                "mail": person.mail_addresses.clone(),
            ]);
        } else if existing_persons.contains_key(name) {
            kanidm_client.delete_entity(ENDPOINT_PERSON, name)?;
        }
    }

    Ok(())
}

fn sync_oauth2s(
    state: &State,
    kanidm_client: &KanidmClient,
    existing_oauth2s: &mut HashMap<String, Value>,
    preexisting_entity_names: &HashSet<String>,
) -> Result<()> {
    log_status("Syncing oauth2 resource servers");
    for (name, oauth2) in &state.systems.oauth2 {
        if oauth2.present {
            if !existing_oauth2s.contains_key(name) {
                if preexisting_entity_names.contains(name) {
                    bail!("Cannot create oauth2 resource server '{name}' because the name is already in use by another entity!");
                }

                kanidm_client.create_entity(
                    &format!("{ENDPOINT_OAUTH2}/_basic"),
                    name,
                    &json!({ "attrs": {
                        "name": [name],
                        "oauth2_rs_origin": [oauth2.origin_url],
                        "displayname": [oauth2.display_name],
                    }}),
                )?;
                existing_oauth2s.clear();
                existing_oauth2s.extend(kanidm_client.get_entities(ENDPOINT_OAUTH2)?);
            }

            if !oauth2.origin_url.ends_with('/') {
                println!("{}", format!("WARN: origin_url ({}) of oauth2 resource server '{name}' should end in a slash! This will lead to unnecessary updates.", oauth2.origin_url).yellow().bold());
            }

            update_oauth2!(kanidm_client, &existing_oauth2s, &name, [
                "displayname": Some(oauth2.display_name.clone()),
                "oauth2_rs_origin": Some(oauth2.origin_url.clone()),
                "oauth2_rs_origin_landing": oauth2.origin_landing.clone(),
                "oauth2_allow_insecure_client_disable_pkce": Some(oauth2.allow_insecure_client_disable_pkce.to_string()),
                "oauth2_prefer_short_username": Some(oauth2.prefer_short_username.to_string()),
            ]);

            for (group, scopes) in &oauth2.scope_maps {
                kanidm_client.update_oauth2_map(
                    "_scopemap",
                    "oauth2_rs_scope_map",
                    existing_oauth2s,
                    name,
                    group,
                    scopes.clone(),
                )?;
            }

            for (group, scopes) in &oauth2.supplementary_scope_maps {
                kanidm_client.update_oauth2_map(
                    "_sup_scopemap",
                    "oauth2_rs_sup_scope_map",
                    existing_oauth2s,
                    name,
                    group,
                    scopes.clone(),
                )?;
            }

            // TODO claim maps
            // TODO secret
        } else if existing_oauth2s.contains_key(name) {
            kanidm_client.delete_entity(ENDPOINT_OAUTH2, name)?;
        }
    }

    Ok(())
}

fn setup_provision_tracking(
    kanidm_client: &KanidmClient,
    existing_groups: &mut HashMap<String, Value>,
) -> Result<HashSet<String>> {
    if !existing_groups.contains_key(PROVISION_TRACKING_GROUP) {
        kanidm_client.create_entity(
            ENDPOINT_GROUP,
            PROVISION_TRACKING_GROUP,
            &json!({ "attrs": { "name": [ PROVISION_TRACKING_GROUP ] } }),
        )?;
        existing_groups.clear();
        existing_groups.extend(kanidm_client.get_entities(ENDPOINT_GROUP)?);
    }

    let entity = existing_groups.get(PROVISION_TRACKING_GROUP).ok_or_else(|| {
        eyre!("Could not find provision tracking group '{PROVISION_TRACKING_GROUP}' in {ENDPOINT_GROUP}")
    })?;

    let mut current_values = match entity.pointer("/attrs/member") {
        Some(Value::Array(x)) => x
            .iter()
            .filter_map(|x| x.as_str())
            .map(|x| x.split_once('@').map(|x| x.0).unwrap_or(x).to_string())
            .collect(),
        None => vec![],
        other => {
            bail!("Invalid attr value for members of entity {ENDPOINT_GROUP}/{PROVISION_TRACKING_GROUP}: {other:?}")
        }
    };

    Ok(HashSet::from_iter(current_values.drain(0..)))
}

fn remove_orphaned_entities(
    kanidm_client: &KanidmClient,
    provisioned_entities: &HashSet<String>,
    existing_groups: &HashMap<String, Value>,
    existing_persons: &HashMap<String, Value>,
    existing_oauth2s: &HashMap<String, Value>,
    tracked_entities: &[String],
) -> Result<()> {
    log_status("Removing orphaned entities");
    // Remove any entities that are no longer provisioned
    let tracked_entities = HashSet::from_iter(tracked_entities.iter().cloned());
    let orphaned_entities = provisioned_entities.difference(&tracked_entities);
    for orphan in orphaned_entities {
        if existing_groups.contains_key(orphan) {
            kanidm_client.delete_entity(ENDPOINT_GROUP, orphan)?;
        } else if existing_persons.contains_key(orphan) {
            kanidm_client.delete_entity(ENDPOINT_PERSON, orphan)?;
        } else if existing_oauth2s.contains_key(orphan) {
            kanidm_client.delete_entity(ENDPOINT_OAUTH2, orphan)?;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Cli::parse();
    let state = State::new(args.state)?;
    let tracked_entities = all_tracked_entities(&state)?;
    let kanidm_client = KanidmClient::new(&args.url, args.accept_invalid_certs)?;

    // Retrieve known entities so we can check for duplicates dynamically
    let mut existing_groups = kanidm_client.get_entities(ENDPOINT_GROUP)?;
    let mut existing_persons = kanidm_client.get_entities(ENDPOINT_PERSON)?;
    let mut existing_oauth2s = kanidm_client.get_entities(ENDPOINT_OAUTH2)?;

    let mut preexisting_entity_names = HashSet::new();
    preexisting_entity_names.extend(existing_groups.keys().cloned());
    preexisting_entity_names.extend(existing_persons.keys().cloned());
    preexisting_entity_names.extend(existing_oauth2s.keys().cloned());

    // Create and query a group that contains all (previously) provisioned entities.
    let provisioned_entities = setup_provision_tracking(&kanidm_client, &mut existing_groups)?;

    sync_groups(&state, &kanidm_client, &mut existing_groups, &preexisting_entity_names)?;
    sync_persons(&state, &kanidm_client, &mut existing_persons, &preexisting_entity_names)?;
    sync_oauth2s(&state, &kanidm_client, &mut existing_oauth2s, &preexisting_entity_names)?;

    // Sync group members
    log_status("Syncing group members");
    for (name, group) in &state.groups {
        if group.present {
            update_attrs!(kanidm_client, ENDPOINT_GROUP, &existing_groups, &name, [
                "member": group.members.clone(),
            ]);
        }
    }

    // Update entity tracking group now that new entities exist.
    // Always add to this group's member, and never overwrite so
    // we can be sure to never lose any entries in case of unexpected errors.
    // Members can thus only be removed by removing the entity itself.
    log_status("Tracking provisioned entities");
    kanidm_client.update_entity_attrs(
        ENDPOINT_GROUP,
        &existing_groups,
        PROVISION_TRACKING_GROUP,
        "member",
        tracked_entities.clone(),
        true,
    )?;

    if args.auto_remove {
        // Now, remove the orphaned entities that were in the tracking group but
        // no longer exist in our state description.
        remove_orphaned_entities(
            &kanidm_client,
            &provisioned_entities,
            &existing_groups,
            &existing_persons,
            &existing_oauth2s,
            &tracked_entities,
        )?;
    }

    Ok(())
}
