//! Relations' tailored CLI grammar. Notes and labels retain their existing
//! literal-string behavior; output delivery is a separate frontend concern.

use super::{render, PeopleFilter, ProfileInput, ProfilePatch, Relations, TieRequest};
use crate::out::Out;
use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use triblespace::prelude::Id;

#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "relations",
    about = "Authored people, organisations, groups, ties between them, and identity verdicts"
)]
pub struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable signing-key file. Reads and writes never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a stable person anchor with an initial profile and active lifecycle.
    Add {
        /// Canonical human-facing label.
        label: String,
        /// Exact stable person id. Omit to mint a fresh anchor.
        #[arg(long, value_parser = parse_id_arg)]
        id: Option<Id>,
        /// Additive provenance label (repeatable).
        #[arg(long)]
        source: Vec<String>,
        #[command(flatten)]
        profile: NewProfileArgs,
    },
    /// Replace selected fields of one current profile snapshot.
    Set {
        /// Person label, alias, exact id, or id prefix.
        person: String,
        /// Additive provenance label (repeatable; does not alter profile identity).
        #[arg(long)]
        source: Vec<String>,
        #[command(flatten)]
        patch: ProfilePatchArgs,
    },
    /// Collapse concurrent profile heads into one explicit successor.
    Reconcile {
        /// Person label, alias, exact id, or id prefix.
        person: String,
        /// Fork head whose full profile is the base. Optional only when every
        /// current head has the same semantic profile value.
        #[arg(long)]
        base: Option<String>,
        #[command(flatten)]
        patch: ProfilePatchArgs,
    },
    /// List person anchors and their current state.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Include settled retired people.
        #[arg(long)]
        all: bool,
        /// Show only settled retired people.
        #[arg(long, conflicts_with = "all")]
        retired: bool,
    },
    /// Show one person or organisation and its ties, including all heads when
    /// a track is forked.
    Show {
        /// Person or organisation label, alias, exact id, or id prefix.
        person: String,
    },
    /// Publish a retired lifecycle successor (also reconciles a lifecycle fork).
    Retire { person: String },
    /// Publish an active lifecycle successor (also reconciles a lifecycle fork).
    #[command(alias = "restore")]
    Unretire { person: String },
    /// Manage addressable exact-member groups.
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Record and inspect explicit same-person/distinct-person verdicts.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Manage organisations: companies, banks, programmes, public bodies.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    /// Record labelled, directed ties between people and organisations.
    Tie {
        #[command(subcommand)]
        command: TieCommand,
    },
    /// List every tie with an entry as subject or object.
    Ties {
        /// Person or organisation label, alias, exact id, or id prefix.
        entry: String,
    },
}

#[derive(Args, Clone, Default)]
struct OrgProfileArgs {
    /// Alternative name or abbreviation (repeatable).
    #[arg(long)]
    alias: Vec<String>,
    #[arg(long)]
    display_name: Option<String>,
    /// Literal note.
    #[arg(long)]
    note: Option<String>,
    /// Website or profile URL (repeatable).
    #[arg(long = "url")]
    url: Vec<String>,
    #[arg(long)]
    email: Vec<String>,
    #[arg(long)]
    phone: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
enum OrgField {
    Aliases,
    DisplayName,
    Note,
    Urls,
    Emails,
    Phones,
}

impl From<OrgField> for super::ProfileField {
    fn from(value: OrgField) -> Self {
        match value {
            OrgField::Aliases => Self::Aliases,
            OrgField::DisplayName => Self::DisplayName,
            OrgField::Note => Self::Note,
            OrgField::Urls => Self::ProfileUrls,
            OrgField::Emails => Self::Emails,
            OrgField::Phones => Self::Phones,
        }
    }
}

#[derive(Subcommand)]
enum OrgCommand {
    /// Add an organisation with an initial profile and active lifecycle.
    Add {
        /// Canonical name.
        label: String,
        /// Additive provenance (repeatable).
        #[arg(long)]
        source: Vec<String>,
        #[command(flatten)]
        profile: OrgProfileArgs,
    },
    /// Replace selected fields of an organisation's current profile.
    Set {
        /// Organisation label, alias, exact id, or id prefix.
        organisation: String,
        #[arg(long)]
        label: Option<String>,
        /// Additive provenance (repeatable; does not alter profile identity).
        #[arg(long)]
        source: Vec<String>,
        /// Present repeated fields replace the complete set.
        #[command(flatten)]
        profile: OrgProfileArgs,
        /// Clear one field. Repeat as needed.
        #[arg(long, value_enum)]
        clear: Vec<OrgField>,
    },
    /// List organisations.
    List {
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Include retired organisations.
        #[arg(long)]
        all: bool,
        /// Show only retired organisations.
        #[arg(long, conflicts_with = "all")]
        retired: bool,
    },
    /// Show one organisation and its ties.
    Show { organisation: String },
    /// Mark an organisation retired (defunct, merged away, out of scope).
    Retire { organisation: String },
    /// Mark an organisation active again.
    Unretire { organisation: String },
}

#[derive(Subcommand)]
enum TieCommand {
    /// Record "SUBJECT RELATION OBJECT", e.g. `"Ada" "works at" "BAB"`. The
    /// same claim recorded again converges on one tie and adds its sources.
    Add {
        /// Person or organisation label, alias, exact id, or id prefix.
        subject: String,
        /// Relation label, read from subject to object ("works at").
        relation: String,
        /// Person or organisation label, alias, exact id, or id prefix.
        object: String,
        /// The subject's role in this tie; part of the claim's identity.
        #[arg(long)]
        role: Option<String>,
        /// Valid since: YYYY, YYYY-MM, YYYY-MM-DD, YYYY-MM-DDTHH:MM, or A..B.
        #[arg(long)]
        since: Option<String>,
        /// Valid until, same forms as --since.
        #[arg(long)]
        until: Option<String>,
        /// Provenance: URL or note (repeatable, accumulates).
        #[arg(long)]
        source: Vec<String>,
        /// Acting persona (relations label or id) for attribution.
        #[arg(long, env = "PERSONA")]
        persona: Option<String>,
    },
    /// Record that a tie ended. The tie stays visible with its end.
    End {
        /// Tie id or unique id prefix.
        tie: String,
        /// When it ended: YYYY, YYYY-MM, YYYY-MM-DD, YYYY-MM-DDTHH:MM, or A..B.
        #[arg(long)]
        until: String,
        /// Provenance for the ending (repeatable).
        #[arg(long)]
        source: Vec<String>,
        /// Acting persona (relations label or id) for attribution.
        #[arg(long, env = "PERSONA")]
        persona: Option<String>,
    },
}

impl OrgProfileArgs {
    fn into_profile(self, label: String) -> ProfileInput {
        ProfileInput {
            label,
            aliases: self.alias,
            display_name: self.display_name,
            note: self.note,
            profile_urls: self.url,
            emails: self.email,
            phones: self.phone,
            ..ProfileInput::default()
        }
    }

    fn into_patch(self, label: Option<String>, clear: Vec<OrgField>) -> ProfilePatch {
        fn provided(values: Vec<String>) -> Option<Vec<String>> {
            (!values.is_empty()).then_some(values)
        }
        ProfilePatch {
            label,
            aliases: provided(self.alias),
            display_name: self.display_name,
            note: self.note,
            profile_urls: provided(self.url),
            emails: provided(self.email),
            phones: provided(self.phone),
            clear: clear.into_iter().map(Into::into).collect(),
            ..ProfilePatch::default()
        }
    }
}

#[derive(Args, Clone, Default)]
struct NewProfileArgs {
    #[arg(long)]
    alias: Vec<String>,
    #[arg(long)]
    affinity: Vec<String>,
    #[arg(long)]
    first_name: Option<String>,
    #[arg(long)]
    last_name: Option<String>,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    teams_user_id: Vec<String>,
    #[arg(long)]
    email: Vec<String>,
    #[arg(long)]
    phone: Vec<String>,
    #[arg(long)]
    company: Option<String>,
    #[arg(long)]
    position: Option<String>,
    #[arg(long)]
    profile_url: Vec<String>,
}

impl NewProfileArgs {
    fn into_profile(self, label: String) -> ProfileInput {
        ProfileInput {
            label,
            aliases: self.alias,
            affinities: self.affinity,
            first_name: self.first_name,
            last_name: self.last_name,
            display_name: self.display_name,
            note: self.note,
            teams_user_ids: self.teams_user_id,
            emails: self.email,
            phones: self.phone,
            company: self.company,
            position: self.position,
            profile_urls: self.profile_url,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
enum ProfileField {
    Aliases,
    Affinities,
    FirstName,
    LastName,
    DisplayName,
    Note,
    TeamsUserIds,
    Emails,
    Phones,
    Company,
    Position,
    ProfileUrls,
}

#[derive(Args, Clone, Default)]
struct ProfilePatchArgs {
    #[arg(long)]
    label: Option<String>,
    /// Replace the complete alias set (repeat for multiple values).
    #[arg(long)]
    alias: Vec<String>,
    /// Replace the complete affinity set (repeat for multiple values).
    #[arg(long)]
    affinity: Vec<String>,
    #[arg(long)]
    first_name: Option<String>,
    #[arg(long)]
    last_name: Option<String>,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    note: Option<String>,
    /// Replace the complete Teams-id set (repeat for multiple values).
    #[arg(long)]
    teams_user_id: Vec<String>,
    /// Replace the complete email set (repeat for multiple values).
    #[arg(long)]
    email: Vec<String>,
    /// Replace the complete phone set (repeat for multiple values).
    #[arg(long)]
    phone: Vec<String>,
    #[arg(long)]
    company: Option<String>,
    #[arg(long)]
    position: Option<String>,
    /// Replace the complete profile-URL set (repeat for multiple values).
    #[arg(long)]
    profile_url: Vec<String>,
    /// Clear one field or complete repeated field. Repeat as needed.
    #[arg(long, value_enum)]
    clear: Vec<ProfileField>,
}

#[derive(Subcommand)]
enum GroupCommand {
    Create {
        name: String,
    },
    Add {
        group: String,
        person: String,
    },
    Remove {
        group: String,
        person: String,
    },
    Rename {
        group: String,
        name: String,
    },
    /// Mark a group as a colony: new task zooids whose spawner belongs to
    /// it get its name as a prefix (`aurora-kuvud-dajiz`). Grants nothing.
    Colony {
        group: String,
    },
    /// Collapse all heads, taking the union of their exact member anchors.
    Reconcile {
        group: String,
        /// Required only when concurrent heads disagree on the name.
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Show {
        group: String,
    },
}

#[derive(Subcommand)]
enum IdentityCommand {
    /// Resolve the current verdict for an unordered person pair.
    Resolve {
        first: String,
        second: String,
        #[arg(
            long,
            conflicts_with = "distinct",
            required_unless_present = "distinct"
        )]
        same: bool,
        #[arg(long, conflicts_with = "same", required_unless_present = "same")]
        distinct: bool,
    },
    /// List canonical person pairs and every live verdict head.
    List,
}

fn parse_id_arg(raw: &str) -> std::result::Result<Id, String> {
    Id::from_hex(raw.trim()).ok_or_else(|| format!("invalid id '{raw}'"))
}

impl From<ProfileField> for super::ProfileField {
    fn from(value: ProfileField) -> Self {
        match value {
            ProfileField::Aliases => Self::Aliases,
            ProfileField::Affinities => Self::Affinities,
            ProfileField::FirstName => Self::FirstName,
            ProfileField::LastName => Self::LastName,
            ProfileField::DisplayName => Self::DisplayName,
            ProfileField::Note => Self::Note,
            ProfileField::TeamsUserIds => Self::TeamsUserIds,
            ProfileField::Emails => Self::Emails,
            ProfileField::Phones => Self::Phones,
            ProfileField::Company => Self::Company,
            ProfileField::Position => Self::Position,
            ProfileField::ProfileUrls => Self::ProfileUrls,
        }
    }
}
impl ProfilePatchArgs {
    fn into_patch(self) -> ProfilePatch {
        fn provided(values: Vec<String>) -> Option<Vec<String>> {
            (!values.is_empty()).then_some(values)
        }
        ProfilePatch {
            label: self.label,
            aliases: provided(self.alias),
            affinities: provided(self.affinity),
            first_name: self.first_name,
            last_name: self.last_name,
            display_name: self.display_name,
            note: self.note,
            teams_user_ids: provided(self.teams_user_id),
            emails: provided(self.email),
            phones: provided(self.phone),
            company: self.company,
            position: self.position,
            profile_urls: provided(self.profile_url),
            clear: self.clear.into_iter().map(Into::into).collect(),
        }
    }
}

pub fn execute(cli: Cli, output: &mut Out<'_>) -> Result<()> {
    let Some(command) = cli.command else {
        return output.line(Cli::command().render_help().to_string());
    };
    let operations = Relations::with_storage(cli.workspace.storage(cli.pile, cli.key)?);
    match command {
        Command::Add {
            label,
            id,
            source,
            profile,
        } => render::added(
            &operations.add(profile.into_profile(label), id, &source)?,
            output,
        ),
        Command::Set {
            person,
            source,
            patch,
        } => render::profile_updated(
            &operations.set(&person, patch.into_patch(), &source)?,
            output,
        ),
        Command::Reconcile {
            person,
            base,
            patch,
        } => render::profile_reconciled(
            &operations.reconcile(&person, base.as_deref(), patch.into_patch())?,
            output,
        ),
        Command::List {
            limit,
            all,
            retired,
        } => {
            let filter = if all {
                PeopleFilter::All
            } else if retired {
                PeopleFilter::Retired
            } else {
                PeopleFilter::Active
            };
            output.noted_text(operations.list(limit, filter)?)
        }
        Command::Show { person } => output.noted_text(operations.show(&person)?),
        Command::Retire { person } => render::lifecycle(&operations.retire(&person)?, true, output),
        Command::Unretire { person } => {
            render::lifecycle(&operations.unretire(&person)?, false, output)
        }
        Command::Group { command } => match command {
            GroupCommand::Create { name } => {
                render::group_created(&operations.group_create(&name)?, output)
            }
            GroupCommand::Add { group, person } => {
                render::group_added(&operations.group_add(&group, &person)?, output)
            }
            GroupCommand::Remove { group, person } => {
                render::group_removed(&operations.group_remove(&group, &person)?, output)
            }
            GroupCommand::Rename { group, name } => {
                render::group_renamed(&operations.group_rename(&group, &name)?, output)
            }
            GroupCommand::Colony { group } => {
                render::group_colony(&operations.group_colony(&group)?, output)
            }
            GroupCommand::Reconcile { group, name } => render::group_reconciled(
                &operations.group_reconcile(&group, name.as_deref())?,
                output,
            ),
            GroupCommand::List => output.noted_text(operations.group_list()?),
            GroupCommand::Show { group } => output.noted_text(operations.group_show(&group)?),
        },
        Command::Identity { command } => match command {
            IdentityCommand::Resolve {
                first,
                second,
                same,
                distinct: _,
            } => render::identity(
                &operations.identity_resolve(&first, &second, same)?,
                same,
                output,
            ),
            IdentityCommand::List => output.noted_text(operations.identity_list()?),
        },
        Command::Org { command } => match command {
            OrgCommand::Add {
                label,
                source,
                profile,
            } => render::organisation_added(
                &operations.org_add(profile.into_profile(label), &source)?,
                output,
            ),
            OrgCommand::Set {
                organisation,
                label,
                source,
                profile,
                clear,
            } => render::profile_updated(
                &operations.org_set(&organisation, profile.into_patch(label, clear), &source)?,
                output,
            ),
            OrgCommand::List {
                limit,
                all,
                retired,
            } => {
                let filter = if all {
                    PeopleFilter::All
                } else if retired {
                    PeopleFilter::Retired
                } else {
                    PeopleFilter::Active
                };
                output.noted_text(operations.org_list(limit, filter)?)
            }
            OrgCommand::Show { organisation } => {
                output.noted_text(operations.org_show(&organisation)?)
            }
            OrgCommand::Retire { organisation } => {
                render::lifecycle(&operations.org_retire(&organisation)?, true, output)
            }
            OrgCommand::Unretire { organisation } => {
                render::lifecycle(&operations.org_unretire(&organisation)?, false, output)
            }
        },
        Command::Tie { command } => match command {
            TieCommand::Add {
                subject,
                relation,
                object,
                role,
                since,
                until,
                source,
                persona,
            } => render::tie_added(
                &operations.tie_add(TieRequest {
                    subject,
                    relation,
                    object,
                    role,
                    since,
                    until,
                    sources: source,
                    persona,
                })?,
                output,
            ),
            TieCommand::End {
                tie,
                until,
                source,
                persona,
            } => render::tie_ended(
                &operations.tie_end(&tie, &until, &source, persona.as_deref())?,
                output,
            ),
        },
        Command::Ties { entry } => output.noted_text(operations.ties(&entry)?),
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    crate::cli::with_output("relations", |output| execute(cli, output))
}
