#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls", "json"] }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! triblespace = "0.16.0"
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{Repository, Workspace};
use triblespace::prelude::blobschemas::LongString;
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, ShortString};
use triblespace::prelude::*;

const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH: &str = "config";

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Auto,
    Tavily,
    Exa,
}

#[derive(Parser)]
#[command(name = "web", about = "Web search/browsing faculty (Tavily/Exa)")]
struct Cli {
    /// Path to the pile file to use.
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name to store web events into (created if missing).
    #[arg(long, default_value = "web", global = true)]
    branch: String,
    /// Branch name to read configuration from.
    #[arg(long, default_value = CONFIG_BRANCH, global = true)]
    config_branch: String,
    /// Override Tavily API key (otherwise loaded from config.tavily_api_key).
    #[arg(long, global = true)]
    tavily_api_key: Option<String>,
    /// Override Exa API key (otherwise loaded from config.exa_api_key).
    #[arg(long, global = true)]
    exa_api_key: Option<String>,
    /// Do not write events to the pile; only print results.
    #[arg(long, global = true)]
    no_store: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Search the web for a query.
    Search {
        query: String,
        #[arg(long, default_value_t = 5)]
        max_results: usize,
        #[arg(long, value_enum, default_value_t = Provider::Auto)]
        provider: Provider,
    },
    /// Fetch and extract a URL (clean text/markdown when supported by provider).
    Fetch {
        url: String,
        #[arg(long, value_enum, default_value_t = Provider::Auto)]
        provider: Provider,
        /// Max characters to return (provider permitting).
        #[arg(long, default_value_t = 12_000)]
        max_characters: usize,
    },
}

mod config_schema {
    use super::*;

    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: NsTAIInterval;
        "328B29CE81665EE719C5A6E91695D4D4" as tavily_api_key: Handle<Blake3, LongString>;
        "AB0DF9F03F28A27A6DB95B693CC0EC53" as exa_api_key: Handle<Blake3, LongString>;
    }
}

const CONFIG_KIND_ID: Id = triblespace::macros::id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

mod web_schema {
    use super::*;

    // Attribute IDs minted with: `trible genid`
    attributes! {
        "0CA16690DE44435B773224C275FD4E76" as query: Handle<Blake3, LongString>;
        "D0A6B39F715FE17935540232656CE0A3" as provider: ShortString;
        "283A66F0FCF94EBCB04DEBF323D2B30D" as created_at: NsTAIInterval;
        "D50E38414AB7068C78602DD56C785634" as result: GenId;

        "099BE36C62777693D66A5F6183ABE9F2" as url: Handle<Blake3, LongString>;
        "A88A91F1F794A30088AB1E4913812D6B" as title: Handle<Blake3, LongString>;
        "6C149EFDDCFEAE8EC101A362035F75D7" as snippet: Handle<Blake3, LongString>;
        "A16BCA98FDE2E8E15F599F3D76E7CDC8" as content: Handle<Blake3, LongString>;
    }

    #[allow(non_upper_case_globals)]
    pub const kind_search: Id = triblespace::macros::id_hex!("0D70C8051CF577A9263CCFBE76027D0A");
    #[allow(non_upper_case_globals)]
    pub const kind_result: Id = triblespace::macros::id_hex!("8BCF14DAAC2CE403666FBE58C4368013");
    #[allow(non_upper_case_globals)]
    pub const kind_fetch: Id = triblespace::macros::id_hex!("91D6FD34AAB1A9C6B24A39D0674F7359");
}

#[derive(Clone, Debug, Default)]
struct ApiKeys {
    tavily: Option<String>,
    exa: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(err) = emit_schema_to_atlas(&cli.pile) {
        eprintln!("atlas emit: {err}");
    }
    let Some(cmd) = cli.command.as_ref() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    let keys = load_api_keys(&cli.pile, &cli.config_branch, &cli.tavily_api_key, &cli.exa_api_key)?;

    match cmd {
        Command::Search {
            query,
            max_results,
            provider,
        } => cmd_search(&cli, keys, *provider, query, *max_results),
        Command::Fetch {
            url,
            provider,
            max_characters,
        } => cmd_fetch(&cli, keys, *provider, url, *max_characters),
    }
}

fn cmd_search(cli: &Cli, keys: ApiKeys, provider: Provider, query: &str, max_results: usize) -> Result<()> {
    let provider = choose_provider(provider, &keys)?;
    let client = Client::builder()
        .user_agent("playground-web-faculty/0")
        .build()
        .context("build http client")?;

    let results = match provider {
        Provider::Tavily => tavily_search(&client, keys.tavily.as_deref().unwrap(), query, max_results)?,
        Provider::Exa => exa_search(&client, keys.exa.as_deref().unwrap(), query, max_results)?,
        Provider::Auto => unreachable!("choose_provider resolves Auto"),
    };

    print_search_results(provider, query, &results);

    if cli.no_store {
        return Ok(());
    }
    store_search(cli, provider, query, &results)
}

fn cmd_fetch(cli: &Cli, keys: ApiKeys, provider: Provider, url: &str, max_characters: usize) -> Result<()> {
    let provider = choose_provider_fetch(provider, &keys)?;
    let client = Client::builder()
        .user_agent("playground-web-faculty/0")
        .build()
        .context("build http client")?;

    let content = match provider {
        Provider::Tavily => {
            tavily_extract(&client, keys.tavily.as_deref().unwrap(), url)?
        }
        Provider::Exa => exa_contents(&client, keys.exa.as_deref().unwrap(), url, max_characters)?,
        Provider::Auto => unreachable!("choose_provider resolves Auto"),
    };

    println!("{content}");

    if cli.no_store {
        return Ok(());
    }
    store_fetch(cli, provider, url, &content)
}

fn choose_provider(provider: Provider, keys: &ApiKeys) -> Result<Provider> {
    match provider {
        Provider::Tavily => {
            if keys.tavily.is_none() {
                bail!("no Tavily API key configured");
            }
            Ok(Provider::Tavily)
        }
        Provider::Exa => {
            if keys.exa.is_none() {
                bail!("no Exa API key configured");
            }
            Ok(Provider::Exa)
        }
        Provider::Auto => {
            if keys.tavily.is_some() {
                Ok(Provider::Tavily)
            } else if keys.exa.is_some() {
                Ok(Provider::Exa)
            } else {
                bail!("no web provider configured (set config.tavily_api_key and/or config.exa_api_key)");
            }
        }
    }
}

fn choose_provider_fetch(provider: Provider, keys: &ApiKeys) -> Result<Provider> {
    match provider {
        Provider::Auto => {
            if keys.exa.is_some() {
                Ok(Provider::Exa)
            } else if keys.tavily.is_some() {
                Ok(Provider::Tavily)
            } else {
                bail!("no web provider configured (set config.tavily_api_key and/or config.exa_api_key)");
            }
        }
        other => choose_provider(other, keys),
    }
}

fn load_api_keys(pile_path: &Path, config_branch: &str, tavily_override: &Option<String>, exa_override: &Option<String>) -> Result<ApiKeys> {
    let debug = std::env::var_os("PLAYGROUND_WEB_DEBUG").is_some();

    let mut keys = ApiKeys {
        tavily: tavily_override.clone(),
        exa: exa_override.clone(),
    };
    if keys.tavily.is_some() && keys.exa.is_some() {
        return Ok(keys);
    }

    let (mut repo, branch_id) = open_repo(pile_path, config_branch)?;
    if debug {
        eprintln!("[web] config branch {config_branch} -> {branch_id:x}");
    }
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull config: {e:?}"))?;
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout config: {e:?}"))?;

    let config_id = latest_config_id(&space)?;
    if debug {
        match config_id {
            Some(id) => eprintln!("[web] latest config id: {id:x}"),
            None => eprintln!("[web] no config entries found"),
        }
    }
    if let Some(config_id) = config_id {
        if keys.tavily.is_none() {
            keys.tavily = load_string_attr(&mut ws, &space, config_id, config_schema::tavily_api_key)?;
        }
        if keys.exa.is_none() {
            keys.exa = load_string_attr(&mut ws, &space, config_id, config_schema::exa_api_key)?;
        }
    }
    if debug {
        eprintln!(
            "[web] keys loaded: tavily={} exa={}",
            keys.tavily.is_some(),
            keys.exa.is_some()
        );
    }

    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;

    Ok(keys)
}

fn latest_config_id(space: &TribleSet) -> Result<Option<Id>> {
    let mut latest: Option<(Id, Value<NsTAIInterval>)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>),
        pattern!(space, [{
            ?config_id @
            config_schema::kind: CONFIG_KIND_ID,
            config_schema::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        match latest {
            Some((_, cur)) if interval_key(cur) >= key => {}
            _ => latest = Some((config_id, updated_at)),
        }
    }
    Ok(latest.map(|(id, _)| id))
}

fn interval_key(value: Value<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = value.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

fn load_string_attr(
    ws: &mut Workspace<Pile<Blake3>>,
    space: &TribleSet,
    entity: Id,
    attr: Attribute<Handle<Blake3, LongString>>,
) -> Result<Option<String>>
{
    let handle = match find!(
        (handle: Value<Handle<Blake3, LongString>>),
        pattern!(space, [{ entity @ attr: ?handle }])
    )
    .into_iter()
    .next()
    {
        Some((handle,)) => handle,
        None => return Ok(None),
    };
    let view: View<str> = ws.get(handle).context("read config string")?;
    Ok(Some(view.to_string()))
}

#[derive(Clone, Debug)]
struct SearchResult {
    url: String,
    title: Option<String>,
    snippet: Option<String>,
}

fn print_search_results(provider: Provider, query: &str, results: &[SearchResult]) {
    let provider_name = match provider {
        Provider::Tavily => "tavily",
        Provider::Exa => "exa",
        Provider::Auto => "auto",
    };
    println!("provider: {provider_name}");
    println!("query: {query}");
    println!("results: {}", results.len());
    println!();
    for (idx, r) in results.iter().enumerate() {
        println!("[{}] {}", idx + 1, r.title.as_deref().unwrap_or("<no title>"));
        println!("url: {}", r.url);
        if let Some(snippet) = r.snippet.as_deref().filter(|s| !s.is_empty()) {
            println!("snippet: {}", snippet.trim());
        }
        println!();
    }
}

fn store_search(cli: &Cli, provider: Provider, query: &str, results: &[SearchResult]) -> Result<()> {
    let (mut repo, branch_id) = open_repo(&cli.pile, &cli.branch)?;
    let mut ws = repo.pull(branch_id).map_err(|e| anyhow!("pull web ws: {e:?}"))?;
    let catalog = ws.checkout(..).map_err(|e| anyhow!("checkout web ws: {e:?}"))?;

    let provider_str = match provider {
        Provider::Tavily => "tavily",
        Provider::Exa => "exa",
        Provider::Auto => "auto",
    };
    let created_at = epoch_interval(now_epoch());
    let query_handle = ws.put(query.to_string());

    let search_fragment = entity! { _ @
        metadata::tag: &web_schema::kind_search,
        web_schema::query: query_handle,
        web_schema::provider: provider_str,
        web_schema::created_at: created_at,
    };
    let search_id = search_fragment
        .root()
        .ok_or_else(|| anyhow!("search fragment missing root export"))?;

    let mut change = TribleSet::new();
    change += search_fragment;

    for r in results {
        let url_handle = ws.put(r.url.clone());
        let result_base = entity! { _ @
            metadata::tag: &web_schema::kind_result,
            web_schema::url: url_handle,
        };
        let result_id = result_base
            .root()
            .ok_or_else(|| anyhow!("result fragment missing root export"))?;
        change += result_base;
        change += entity! { ExclusiveId::force_ref(&search_id) @ web_schema::result: result_id };

        if let Some(title) = r.title.as_deref().filter(|s| !s.is_empty()) {
            change += entity! { ExclusiveId::force_ref(&result_id) @ web_schema::title: ws.put(title.to_string()) };
        }
        if let Some(snippet) = r.snippet.as_deref().filter(|s| !s.is_empty()) {
            change += entity! { ExclusiveId::force_ref(&result_id) @ web_schema::snippet: ws.put(snippet.to_string()) };
        }
    }

    let delta = change.difference(&catalog);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("web search"));
        push_workspace(&mut repo, &mut ws).context("push web search")?;
    }

    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;

    Ok(())
}

fn store_fetch(cli: &Cli, provider: Provider, url: &str, content: &str) -> Result<()> {
    let (mut repo, branch_id) = open_repo(&cli.pile, &cli.branch)?;
    let mut ws = repo.pull(branch_id).map_err(|e| anyhow!("pull web ws: {e:?}"))?;
    let catalog = ws.checkout(..).map_err(|e| anyhow!("checkout web ws: {e:?}"))?;

    let provider_str = match provider {
        Provider::Tavily => "tavily",
        Provider::Exa => "exa",
        Provider::Auto => "auto",
    };
    let created_at = epoch_interval(now_epoch());
    let url_handle = ws.put(url.to_string());
    let content_handle = ws.put(content.to_string());

    let fetch_fragment = entity! { _ @
        metadata::tag: &web_schema::kind_fetch,
        web_schema::provider: provider_str,
        web_schema::created_at: created_at,
        web_schema::url: url_handle,
        web_schema::content: content_handle,
    };

    let delta = fetch_fragment.difference(&catalog);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("web fetch"));
        push_workspace(&mut repo, &mut ws).context("push web fetch")?;
    }

    repo.close()
        .map_err(|e| anyhow!("close pile: {e:?}"))?;

    Ok(())
}

fn push_workspace(repo: &mut Repository<Pile<Blake3>>, ws: &mut Workspace<Pile<Blake3>>) -> Result<()> {
    while let Some(mut conflict) = repo.try_push(ws).map_err(|e| anyhow!("push: {e:?}"))? {
        conflict
            .merge(ws)
            .map_err(|e| anyhow!("merge conflict: {e:?}"))?;
        *ws = conflict;
    }
    Ok(())
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).to_value()
}

// --- Tavily ---

#[derive(Deserialize)]
struct TavilySearchResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

fn tavily_search(client: &Client, api_key: &str, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
    let resp: TavilySearchResponse = client
        .post("https://api.tavily.com/search")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .json(&json!({
            "query": query,
            "search_depth": "basic",
            "max_results": max_results,
            "include_answer": false,
            "include_raw_content": false,
        }))
        .send()
        .context("tavily search request")?
        .error_for_status()
        .context("tavily search status")?
        .json()
        .context("tavily search json")?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchResult {
            url: r.url,
            title: Some(r.title).filter(|s| !s.is_empty()),
            snippet: Some(r.content).filter(|s| !s.is_empty()),
        })
        .collect())
}

#[derive(Deserialize)]
struct TavilyExtractResponse {
    results: Vec<TavilyExtractResult>,
}

#[derive(Deserialize)]
struct TavilyExtractResult {
    url: String,
    #[serde(default)]
    raw_content: String,
    #[serde(default)]
    content: String,
}

fn tavily_extract(client: &Client, api_key: &str, url: &str) -> Result<String> {
    let resp: TavilyExtractResponse = client
        .post("https://api.tavily.com/extract")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .json(&json!({
            "urls": [url],
            "extract_depth": "basic",
            "format": "markdown",
        }))
        .send()
        .context("tavily extract request")?
        .error_for_status()
        .context("tavily extract status")?
        .json()
        .context("tavily extract json")?;

    let Some(first) = resp.results.into_iter().next() else {
        bail!("tavily extract returned no results");
    };
    let text = if !first.raw_content.is_empty() {
        first.raw_content
    } else {
        first.content
    };
    Ok(text)
}

// --- Exa ---

#[derive(Deserialize)]
struct ExaSearchResponse {
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
}

fn exa_search(client: &Client, api_key: &str, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
    let resp: ExaSearchResponse = client
        .post("https://api.exa.ai/search")
        .header(CONTENT_TYPE, "application/json")
        .header("x-api-key", api_key)
        .json(&json!({
            "query": query,
            "numResults": max_results,
            "text": false,
        }))
        .send()
        .context("exa search request")?
        .error_for_status()
        .context("exa search status")?
        .json()
        .context("exa search json")?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchResult {
            url: r.url,
            title: Some(r.title).filter(|s| !s.is_empty()),
            snippet: Some(r.text).filter(|s| !s.is_empty()),
        })
        .collect())
}

#[derive(Deserialize)]
struct ExaContentsResponse {
    results: Vec<ExaContentsResult>,
}

#[derive(Deserialize)]
struct ExaContentsResult {
    url: String,
    #[serde(default)]
    text: String,
}

fn exa_contents(client: &Client, api_key: &str, url: &str, max_characters: usize) -> Result<String> {
    let resp: ExaContentsResponse = client
        .post("https://api.exa.ai/contents")
        .header(CONTENT_TYPE, "application/json")
        .header("x-api-key", api_key)
        .json(&json!({
            "urls": [url],
            "text": {
                "maxCharacters": max_characters,
                "includeHtmlTags": false,
            },
        }))
        .send()
        .context("exa contents request")?
        .error_for_status()
        .context("exa contents status")?
        .json()
        .context("exa contents json")?;

    let Some(first) = resp.results.into_iter().next() else {
        bail!("exa contents returned no results");
    };
    Ok(first.text)
}

// --- Atlas schema metadata ---

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile_path, ATLAS_BRANCH)?;
    let metadata = build_web_metadata(repo.storage_mut())
        .map_err(|e| anyhow!("build web metadata: {e:?}"))?;

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull atlas: {e:?}"))?;
    let space = ws.checkout(..).map_err(|e| anyhow!("checkout atlas: {e:?}"))?;
    let delta = metadata.difference(&space);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("atlas schema metadata"));
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push atlas metadata: {e:?}"))?;
    }
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn build_web_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
{
    let mut out = TribleSet::new();

    out += <GenId as metadata::ConstDescribe>::describe(blobs)?;
    out += <ShortString as metadata::ConstDescribe>::describe(blobs)?;
    out += <NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
    out += <Handle<Blake3, LongString> as metadata::ConstDescribe>::describe(blobs)?;
    out += <LongString as metadata::ConstDescribe>::describe(blobs)?;

    out += describe_attribute(blobs, &web_schema::query, "web_query")?;
    out += describe_attribute(blobs, &web_schema::provider, "web_provider")?;
    out += describe_attribute(blobs, &web_schema::created_at, "web_created_at")?;
    out += describe_attribute(blobs, &web_schema::result, "web_result")?;
    out += describe_attribute(blobs, &web_schema::url, "web_url")?;
    out += describe_attribute(blobs, &web_schema::title, "web_title")?;
    out += describe_attribute(blobs, &web_schema::snippet, "web_snippet")?;
    out += describe_attribute(blobs, &web_schema::content, "web_content")?;

    out += describe_kind(blobs, &web_schema::kind_search, "web_kind_search", "Web search event kind.")?;
    out += describe_kind(blobs, &web_schema::kind_result, "web_kind_result", "Web result entity kind.")?;
    out += describe_kind(blobs, &web_schema::kind_fetch, "web_kind_fetch", "Web fetch/extract event kind.")?;

    Ok(out)
}

fn describe_attribute<B, S>(
    blobs: &mut B,
    attribute: &Attribute<S>,
    name: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
    S: ValueSchema,
{
    let mut tribles = TribleSet::new();
    tribles += metadata::Describe::describe(attribute, blobs)?;
    let handle = blobs.put(name.to_owned())?;
    let attribute_id = attribute.id();
    tribles += entity! { ExclusiveId::force_ref(&attribute_id) @ metadata::name: handle };
    Ok(tribles)
}

fn describe_kind<B>(
    blobs: &mut B,
    id: &Id,
    name: &str,
    description: &str,
) -> std::result::Result<Fragment, B::PutError>
where
    B: BlobStore<Blake3>,
{
    Ok(entity! { ExclusiveId::force_ref(id) @
        metadata::name: blobs.put(name.to_string())?,
        metadata::description: blobs.put(description.to_string())?,
    })
}

// --- Pile helpers ---

fn open_repo(path: &Path, branch_name: &str) -> Result<(Repository<Pile<Blake3>>, Id)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile = Pile::<Blake3>::open(path)
        .map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    pile.restore()
        .map_err(|e| anyhow!("restore pile {}: {e:?}", path.display()))?;

    let existing = find_branch_by_name(&mut pile, branch_name)?;
    let signing_key = SigningKey::generate(&mut OsRng);
    let mut repo = Repository::new(pile, signing_key);
    let branch_id = match existing {
        Some(id) => id,
        None => repo
            .create_branch(branch_name, None)
            .map_err(|e| anyhow!("create branch: {e:?}"))?
            .release(),
    };
    Ok((repo, branch_id))
}

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let expected_name_handle = branch_name.to_owned().to_blob().get_handle::<Blake3>();
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile.branches().map_err(|e| anyhow!("list branches: {e:?}"))?;

    let mut fallback: Option<Id> = None;
    for bid in iter {
        let bid = bid?;
        let Some(meta_handle) = pile.head(bid)? else {
            continue;
        };
        let meta: TribleSet = reader
            .get::<TribleSet, blobschemas::SimpleArchive>(meta_handle)
            .map_err(|e| anyhow!("load branch metadata: {e:?}"))?;
        let mut names = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(&meta, [{ metadata::name: ?handle }])
        )
        .into_iter();
        let Some(name) = names.next().map(|(handle,)| handle) else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if name.raw != expected_name_handle.raw {
            continue;
        }

        // Prefer branches that already have a commit head set (non-empty branch).
        // Otherwise, fall back to any matching branch.
        let has_commit_head = find!(
            (handle: Value<Handle<Blake3, blobschemas::SimpleArchive>>),
            pattern!(&meta, [{ triblespace::core::repo::head: ?handle }])
        )
        .into_iter()
        .next()
        .is_some();
        if has_commit_head {
            return Ok(Some(bid));
        }
        if fallback.is_none() {
            fallback = Some(bid);
        }
    }
    Ok(fallback)
}
