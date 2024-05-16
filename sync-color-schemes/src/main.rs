use crate::scheme::Scheme;
use anyhow::Context;
use config::{ColorSchemeFile, ColorSchemeMetaData};
use serde::Deserialize;
use sqlite_cache::Cache;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;
use tar::Archive;
use tempfile::NamedTempFile;

mod base16;
mod gogh;
mod iterm2;
mod scheme;
mod sexy;

lazy_static::lazy_static! {
    static ref CACHE: Cache = make_cache();
}

fn apply_nightly_version(metadata: &mut ColorSchemeMetaData) {
    metadata
        .wezterm_version
        .replace("nightly builds only".to_string());
}

fn make_cache() -> Cache {
    let file_name = "/tmp/wezterm-sync-color-schemes.sqlite";
    let connection = sqlite_cache::rusqlite::Connection::open(&file_name).unwrap();
    Cache::new(sqlite_cache::CacheConfig::default(), connection).unwrap()
}

pub async fn fetch_url_as_str(url: &str) -> anyhow::Result<String> {
    let data = fetch_url(url)
        .await
        .with_context(|| format!("fetching {url}"))?;
    String::from_utf8(data).with_context(|| format!("converting data from {url} to string"))
}

pub async fn fetch_url(url: &str) -> anyhow::Result<Vec<u8>> {
    let topic = CACHE.topic("data-by-url").context("creating cache topic")?;

    let (updater, item) = topic
        .get_for_update(url)
        .await
        .context("lookup url in cache")?;
    if let Some(item) = item {
        return Ok(item.data);
    }

    println!("Going to request {url}");
    let client = reqwest::Client::builder()
        .user_agent("wezterm-sync-color-schemes/1.0")
        .build()?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let mut ttl = Duration::from_secs(86400);
    if let Some(value) = response.headers().get(reqwest::header::CACHE_CONTROL) {
        if let Ok(value) = value.to_str() {
            let fields = value.splitn(2, "=").collect::<Vec<_>>();
            if fields.len() == 2 && fields[0] == "max-age" {
                if let Ok(secs) = fields[1].parse::<u64>() {
                    ttl = Duration::from_secs(secs);
                }
            }
        }
    }

    let status = response.status();

    let data = response.bytes().await?.to_vec();

    if status != reqwest::StatusCode::OK {
        anyhow::bail!("{}", String::from_utf8_lossy(&data));
    }

    updater.write(&data, ttl).context("assigning to cache")?;
    Ok(data)
}

fn make_ident(key: &str) -> String {
    let key = key.to_ascii_lowercase();
    let fields: Vec<&str> = key
        .split(|c: char| !c.is_alphanumeric())
        .filter(|c| !c.is_empty())
        .collect();
    fields.join("-")
}

fn make_prefix(key: &str) -> (char, String) {
    for c in key.chars() {
        match c {
            '0'..='9' | 'a'..='z' => return (c, key.to_ascii_lowercase()),
            'A'..='Z' => return (c.to_ascii_lowercase(), key.to_ascii_lowercase()),
            _ => continue,
        }
    }
    panic!("no good prefix");
}

const DATA_FILE_NAME: &str = "docs/colorschemes/data.json";
fn bake_for_config(schemeses: SchemeSet) -> anyhow::Result<()> {
    let mut all = vec![];
    for s in schemeses.by_name.values() {
        // Only interested in aliases with different-enough names
        let mut aliases = s.data.metadata.aliases.clone();

        aliases.sort();
        aliases.dedup();
        aliases.retain(|name| name != &s.name);

        let mut s = s.clone();
        s.data.metadata.aliases = aliases.clone();

        all.push(s.clone());
    }
    all.sort_by_key(|s| make_prefix(&s.name));

    let count = all.len();
    let mut code = String::new();
    code.push_str(&format!(
        "//! This file was generated by sync-color-schemes\n
pub const SCHEMES: [(&'static str, &'static str); {count}] = [\n
    // Start here
",
    ));

    for s in &all {
        let name = s.name.escape_default();
        let toml = s.to_toml()?;
        let toml = toml.escape_default();
        code.push_str(&format!("(\"{name}\", \"{toml}\"),\n",));
    }
    code.push_str("];\n");

    {
        let file_name = "config/src/scheme_data.rs";
        let update = match std::fs::read_to_string(file_name) {
            Ok(existing) => existing != code,
            Err(_) => true,
        };

        if update {
            println!("Updating {file_name}");
            std::fs::write(file_name, code)?;
        }
    }

    // Summarize new schemes for the changelog
    let mut new_items = vec![];
    for s in &all {
        if s.data.metadata.wezterm_version.as_deref() == Some("nightly builds only") {
            let (prefix, _) = make_prefix(&s.name);
            let ident = make_ident(&s.name);
            new_items.push(format!(
                "[{}](colorschemes/{}/index.md#{})",
                s.name, prefix, ident
            ));
        }
    }
    if !new_items.is_empty() {
        println!("* Color schemes: {}", new_items.join(",\n  "));
    }

    // And the data for the docs

    let mut doc_data = vec![];
    for s in all {
        doc_data.push(s.to_json_value()?);
    }

    let json = serde_json::to_string_pretty(&doc_data)?;
    let update = match std::fs::read_to_string(&DATA_FILE_NAME) {
        Ok(existing) => existing != json,
        Err(_) => true,
    };

    if update {
        println!("Updating {DATA_FILE_NAME}");
        std::fs::write(&DATA_FILE_NAME, json)?;
    }

    Ok(())
}

struct SchemeSet {
    by_name: HashMap<String, Scheme>,
    version_by_color_scheme: BTreeMap<String, String>,
    version_by_name: BTreeMap<String, String>,
}

impl SchemeSet {
    pub fn accumulate(&mut self, to_add: Vec<Scheme>) {
        for candidate in to_add {
            self.add(candidate);
        }
    }

    pub fn load_existing() -> anyhow::Result<Self> {
        let mut by_name = HashMap::new();
        let mut version_by_color_scheme = BTreeMap::new();
        let mut names_by_color_scheme = BTreeMap::new();
        let mut version_by_name = BTreeMap::new();

        if let Ok(data) = std::fs::read_to_string(&DATA_FILE_NAME) {
            #[derive(Deserialize)]
            struct Entry {
                colors: serde_json::Value,
                metadata: MetaData,
            }
            #[derive(Deserialize)]
            struct MetaData {
                name: String,
                aliases: Vec<String>,
                wezterm_version: Option<String>,
            }

            let existing: Vec<Entry> = serde_json::from_str(&data)?;
            for item in existing {
                if let Some(version) = &item.metadata.wezterm_version {
                    let ident = serde_json::to_string(&item.colors)?;
                    version_by_color_scheme.insert(ident.to_string(), version.to_string());
                    version_by_name.insert(item.metadata.name.to_string(), version.to_string());

                    let mut names = item.metadata.aliases;
                    names.insert(0, item.metadata.name);

                    for name in names {
                        names_by_color_scheme
                            .entry(ident.to_string())
                            .or_insert_with(Vec::new)
                            .push(name);
                    }
                }
            }

            let existing: Vec<serde_json::Value> = serde_json::from_str(&data)?;
            for item in existing {
                let data = ColorSchemeFile::from_json_value(&item)?;
                if data.colors.ansi.is_none() {
                    continue;
                }
                let name = data.metadata.name.as_ref().unwrap().to_string();
                by_name.insert(
                    name.to_string(),
                    Scheme {
                        name,
                        file_name: None,
                        data,
                    },
                );
            }
        }

        Ok(Self {
            by_name,
            version_by_color_scheme,
            version_by_name,
        })
    }

    pub fn add(&mut self, mut candidate: Scheme) {
        for existing in self.by_name.values_mut() {
            if candidate == *existing || candidate.data.colors == existing.data.colors {
                log::info!("Adding {} as alias of {}", candidate.name, existing.name);
                existing.data.metadata.aliases.push(candidate.name.clone());
                return;
            }
        }

        // Resolve wezterm version information for this scheme
        let json = candidate.to_json_value().expect("scheme to be json compat");
        let ident =
            serde_json::to_string(json.get("colors").unwrap()).expect("colors to be json compat");
        if let Some(version) = self
            .version_by_color_scheme
            .get(&ident)
            .or_else(|| self.version_by_name.get(&candidate.name))
            .or_else(|| {
                for a in &candidate.data.metadata.aliases {
                    if let Some(v) = self.version_by_name.get(a) {
                        return Some(v);
                    }
                }
                None
            })
        {
            candidate
                .data
                .metadata
                .wezterm_version
                .replace(version.to_string());
        }

        if let Some(existing) = self.by_name.remove(&candidate.name) {
            // Already exists but we didn't find it by exact color match
            // above.
            candidate.data.metadata.aliases = existing.data.metadata.aliases;
            println!("Updating {}", candidate.name);
        } else {
            println!("Adding {}", candidate.name);
        }
        self.by_name.insert(candidate.name.to_string(), candidate);
    }

    async fn sync_toml(
        &mut self,
        repo_url: &str,
        branch: &str,
        suffix: &str,
    ) -> anyhow::Result<()> {
        let tarball_url = if repo_url.starts_with("https://codeberg.org/") {
            format!("{repo_url}/archive/{branch}.tar.gz")
        } else {
            format!("{repo_url}/tarball/{branch}")
        };
        let tar_data = fetch_url(&tarball_url).await?;
        let decoder = libflate::gzip::Decoder::new(tar_data.as_slice())?;
        let mut tar = Archive::new(decoder);
        for entry in tar.entries()? {
            let mut entry = entry?;

            if entry
                .path()?
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "toml")
                .unwrap_or(false)
            {
                let dest_file = NamedTempFile::new()?;
                entry.unpack(dest_file.path())?;

                let data = std::fs::read_to_string(dest_file.path())?;

                match ColorSchemeFile::from_toml_str(&data) {
                    Ok(mut scheme) => {
                        let name = match scheme.metadata.name {
                            Some(name) => name,
                            None => entry
                                .path()?
                                .file_stem()
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .to_string(),
                        };
                        let name = format!("{name}{suffix}");
                        scheme.metadata.name = Some(name.clone());
                        if scheme.metadata.origin_url.is_none() {
                            scheme.metadata.origin_url = Some(repo_url.to_string());
                        }
                        apply_nightly_version(&mut scheme.metadata);

                        let scheme = Scheme {
                            name: name.clone(),
                            file_name: None,
                            data: scheme,
                        };

                        self.add(scheme);
                    }
                    Err(err) => {
                        log::error!("{tarball_url}/{}: {err:#}", entry.path().unwrap().display());
                    }
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // They color us! my precious!
    let mut schemeses = SchemeSet::load_existing()?;

    schemeses
        .sync_toml("https://github.com/catppuccin/wezterm", "main", "")
        .await?;
    schemeses
        .sync_toml("https://github.com/EdenEast/nightfox.nvim", "main", "")
        .await?;
    schemeses
        .sync_toml(
            "https://github.com/Hiroya-W/wezterm-sequoia-theme",
            "main",
            "",
        )
        .await?;
    schemeses
        .sync_toml("https://github.com/dracula/wezterm", "main", "")
        .await?;
    schemeses
        .sync_toml(
            "https://github.com/olivercederborg/poimandres.nvim",
            "main",
            "",
        )
        .await?;
    schemeses
        .sync_toml("https://github.com/folke/tokyonight.nvim", "main", "")
        .await?;
    schemeses
        .sync_toml("https://codeberg.org/anhsirk0/wezterm-themes", "main", "")
        .await?;
    schemeses
        .sync_toml(
            "https://github.com/hardhackerlabs/theme-wezterm",
            "master",
            "",
        )
        .await?;
    schemeses
        .sync_toml("https://github.com/ribru17/bamboo.nvim", "master", "")
        .await?;
    schemeses
        .sync_toml("https://github.com/eldritch-theme/wezterm", "master", "")
        .await?;
    schemeses.accumulate(iterm2::sync_iterm2().await.context("sync iterm2")?);
    schemeses.accumulate(base16::sync().await.context("sync base16")?);
    schemeses.accumulate(gogh::sync_gogh().await.context("sync gogh")?);
    schemeses.accumulate(sexy::sync_sexy().await.context("sync sexy")?);
    bake_for_config(schemeses)?;

    Ok(())
}
