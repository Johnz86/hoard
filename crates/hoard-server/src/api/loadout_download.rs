use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use crate::api::common::shell_quoted;
use crate::api::loadouts::{LoadoutView, view_of};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DownloadParams {
    format: Option<String>,
    root: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DownloadScript {
    filename: String,
    text: String,
    layout: String,
}

pub(crate) async fn script(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<DownloadParams>,
) -> ApiResult<Json<DownloadScript>> {
    let id = id.parse().map_err(|_| ApiError::not_found("loadout"))?;
    let catalog = std::sync::Arc::clone(state.catalog());
    let node = state.node_name().to_owned();
    let view = crate::api::common::blocking(move || view_of(&catalog, id, &node)).await?;
    validate(&view)?;
    let root = params.root.as_deref().unwrap_or("./models");
    if root.trim().is_empty() || root.chars().any(char::is_control) {
        return Err(ApiError::bad_request("Choose a client download folder"));
    }
    let format = params.format.as_deref().unwrap_or("powershell");
    let (extension, text) = match format {
        "powershell" => ("ps1", powershell(&view, &state.public_url(), root)?),
        "shell" => ("sh", shell(&view, &state.public_url(), root)?),
        _ => return Err(ApiError::bad_request("Choose PowerShell or POSIX shell")),
    };
    Ok(Json(DownloadScript {
        filename: format!("hoard-loadout-{id}.{extension}"),
        text,
        layout: layout(&view, root),
    }))
}

fn layout(view: &LoadoutView, root: &str) -> String {
    let mut folders = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for item in &view.items {
        let (folder, name) = item
            .destination
            .as_str()
            .rsplit_once('/')
            .unwrap_or(("", item.destination.as_str()));
        folders.entry(folder).or_default().push(name);
    }
    let mut text = format!("{root}\n");
    for (folder, names) in folders {
        if !folder.is_empty() {
            text.push_str(&format!("  {folder}/\n"));
        }
        for name in names {
            text.push_str(&format!("    {name}\n"));
        }
    }
    text
}

fn validate(view: &LoadoutView) -> ApiResult<()> {
    if view.items.is_empty() {
        return Err(ApiError::conflict(
            "Add models before downloading this loadout",
        ));
    }
    let mut paths = BTreeSet::new();
    for item in &view.items {
        if !paths.insert(item.destination.as_str().to_lowercase()) {
            return Err(ApiError::conflict(format!(
                "Two models use {}. Edit their download paths before downloading.",
                item.destination.as_str()
            )));
        }
        match &item.download_source {
            None => {
                return Err(ApiError::conflict(format!(
                    "{} is unavailable on this server. Restore its source before downloading.",
                    item.name
                )));
            }
            Some(source) if source.multi_file => {
                return Err(ApiError::conflict(format!(
                    "{} contains multiple shards. Collection scripts currently require individual model files.",
                    item.name
                )));
            }
            Some(source) if !source.hash_ready => {
                return Err(ApiError::conflict(format!(
                    "{} cannot be downloaded yet: its content hash is unavailable. Finish hashing the source first.",
                    item.name
                )));
            }
            Some(source) => {
                hoard_core::DownloadPath::parse(&source.path)?;
            }
        }
    }
    Ok(())
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn powershell(view: &LoadoutView, endpoint: &str, root: &str) -> ApiResult<String> {
    let mut text = format!(
        "$ErrorActionPreference = 'Stop'\n$env:HF_ENDPOINT = {}\n$root = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath({})\n",
        ps_quote(endpoint),
        ps_quote(root)
    );
    text.push_str(r#"
if (-not (Get-Command hf -ErrorAction SilentlyContinue)) { throw 'Install the Hugging Face hf CLI first.' }
function Get-HoardModel($Repo, $File, $Revision, $RepoType, $RelativePath, $StageName) {
    $target = Join-Path $root $RelativePath
    if (Test-Path -LiteralPath $target) {
        Write-Warning "Preserved existing file (not validated): $target"
        return
    }
    $stage = Join-Path $root ('.hoard-downloads/' + $StageName)
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
    & hf download --repo-type $RepoType --revision $Revision --local-dir $stage -- $Repo $File
    if ($LASTEXITCODE -ne 0) { throw "Download failed: $File" }
    Move-Item -LiteralPath (Join-Path $stage $File) -Destination $target
    Write-Host "Saved: $target"
}

"#);
    for (index, item) in view.items.iter().enumerate() {
        let source = item
            .download_source
            .as_ref()
            .ok_or_else(|| ApiError::conflict("A model is unavailable"))?;
        text.push_str(&format!(
            "Get-HoardModel {} {} {} {} {} {}\n",
            ps_quote(&source.repo.reference()),
            ps_quote(&source.path),
            ps_quote(&source.revision),
            ps_quote(source.repo.repo_type().singular()),
            ps_quote(item.destination.as_str()),
            ps_quote(&format!("{}-{index}", view.id))
        ));
    }
    Ok(text)
}

fn shell(view: &LoadoutView, endpoint: &str, root: &str) -> ApiResult<String> {
    let mut text = format!(
        "#!/bin/sh\nset -eu\nexport HF_ENDPOINT={}\nroot={}\n",
        shell_quoted(endpoint),
        shell_quoted(root)
    );
    text.push_str(r#"
command -v hf >/dev/null 2>&1 || { printf '%s\n' 'Install the Hugging Face hf CLI first.' >&2; exit 1; }
get_hoard_model() {
    target="$root/$5"
    if [ -e "$target" ] || [ -L "$target" ]; then
        printf 'Preserved existing file (not validated): %s\n' "$target" >&2
        return
    fi
    stage="$root/.hoard-downloads/$6"
    mkdir -p -- "$(dirname -- "$target")"
    hf download --repo-type "$4" --revision "$3" --local-dir "$stage" -- "$1" "$2"
    if [ -e "$target" ] || [ -L "$target" ]; then
        printf 'Destination appeared during download: %s\n' "$target" >&2
        exit 1
    fi
    mv -n -- "$stage/$2" "$target"
    printf 'Saved: %s\n' "$target"
}

"#);
    for (index, item) in view.items.iter().enumerate() {
        let source = item
            .download_source
            .as_ref()
            .ok_or_else(|| ApiError::conflict("A model is unavailable"))?;
        text.push_str(&format!(
            "get_hoard_model {} {} {} {} {} {}\n",
            shell_quoted(&source.repo.reference()),
            shell_quoted(&source.path),
            shell_quoted(&source.revision),
            shell_quoted(source.repo.repo_type().singular()),
            shell_quoted(item.destination.as_str()),
            shell_quoted(&format!("{}-{index}", view.id))
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_literals_do_not_expand_or_execute_user_input() {
        assert_eq!(
            ps_quote("E:\\Models $x; 'test'"),
            "'E:\\Models $x; ''test'''"
        );
    }
}
