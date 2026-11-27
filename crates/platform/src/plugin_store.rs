//! Isolated storage, discovery, pure-Rust archive extraction, and atomic
//! state persistence for installable PerformanceHub plugins.
use omen_core::{
    Inventory,
    plugins::{
        InstalledPlugin, MAX_PLUGIN_FILES, MAX_PLUGIN_UNPACKED_BYTES, PLUGINS_STATE_SCHEMA,
        PluginManifest, PluginStateEntry, PluginStatus, PluginsStateFile, validate_relative_path,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NONCE: AtomicU64 = AtomicU64::new(1);

fn next_nonce() -> String {
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{pid}-{ts}-{n}")
}

fn unix_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed(PluginManifest),
    NeedsReplaceConfirm(PluginManifest),
}

pub struct PluginStore {
    root: PathBuf,
}

impl PluginStore {
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    #[cfg(windows)]
    pub fn user_data() -> Result<Self, String> {
        Ok(Self::at(crate::windows::profile_directory()?))
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("plugins.json")
    }

    pub fn ensure_plugins_dir(&self) -> Result<PathBuf, String> {
        let dir = self.plugins_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("create plugins dir: {e}"))?;
        Ok(dir)
    }

    pub fn load_state(&self) -> Result<PluginsStateFile, String> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(PluginsStateFile::default());
        }
        let raw = fs::read_to_string(&path).map_err(|e| format!("read plugins.json: {e}"))?;
        let state: PluginsStateFile =
            serde_json::from_str(&raw).map_err(|e| format!("parse plugins.json: {e}"))?;
        if state.schema != PLUGINS_STATE_SCHEMA {
            return Err(format!("unsupported plugins.json schema {}", state.schema));
        }
        Ok(state)
    }

    pub fn save_state(&self, state: &PluginsStateFile) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|e| format!("create data dir: {e}"))?;
        let path = self.state_path();
        let tmp = self.root.join(format!(".plugins-{}.tmp", next_nonce()));
        let bytes =
            serde_json::to_vec_pretty(state).map_err(|e| format!("serialize plugins.json: {e}"))?;
        fs::write(&tmp, &bytes).map_err(|e| format!("write temp plugins.json: {e}"))?;
        if let Err(err) = fs::rename(&tmp, &path) {
            if path.exists() {
                let _ = fs::remove_file(&path);
                if let Err(err2) = fs::rename(&tmp, &path) {
                    let _ = fs::remove_file(&tmp);
                    return Err(format!("commit plugins.json: {err2}"));
                }
            } else {
                let _ = fs::remove_file(&tmp);
                return Err(format!("commit plugins.json: {err}"));
            }
        }
        Ok(())
    }

    /// Scans `%USERPROFILE%\.performance-hub\plugins\` and returns every
    /// installed plugin sorted deterministically by `(nav_order, id)`.
    pub fn scan(&self, inventory: &Inventory) -> Vec<InstalledPlugin> {
        let dir = self.plugins_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return vec![];
        };
        let state = self.load_state().unwrap_or_default();
        let mut folders: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    return None;
                }
                let path = e.path();
                let meta = fs::symlink_metadata(&path).ok()?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return None;
                }
                Some((name, path))
            })
            .collect();
        folders.sort_by(|a, b| a.0.cmp(&b.0));

        // First pass: parse manifests and detect duplicate plugin IDs.
        let mut parsed: Vec<(String, Result<PluginManifest, String>)> = Vec::new();
        let mut id_folders: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (folder_name, path) in folders {
            let manifest_path = path.join("plugin.json");
            let res = read_and_validate_manifest(&manifest_path);
            if let Ok(ref m) = res {
                id_folders
                    .entry(m.id.clone())
                    .or_default()
                    .push(folder_name.clone());
            }
            parsed.push((folder_name, res));
        }

        let mut results = Vec::new();
        for (folder_name, res) in parsed {
            match res {
                Err(err) => {
                    results.push(InstalledPlugin {
                        id: format!("invalid-{folder_name}"),
                        folder_name,
                        manifest: None,
                        status: PluginStatus::Error(err.clone()),
                        enabled: false,
                        can_toggle: false,
                        can_remove: true,
                        diagnostic: Some(err),
                        pinned_sha256: None,
                    });
                }
                Ok(manifest) => {
                    let duplicates = id_folders.get(&manifest.id).map_or(0, std::vec::Vec::len);
                    let is_duplicate_loser = if duplicates > 1 {
                        let exact_exists = id_folders
                            .get(&manifest.id)
                            .is_some_and(|v| v.iter().any(|f| f == &manifest.id));
                        if exact_exists {
                            folder_name != manifest.id
                        } else {
                            id_folders
                                .get(&manifest.id)
                                .and_then(|v| v.first())
                                .is_some_and(|first| first != &folder_name)
                        }
                    } else {
                        false
                    };
                    if is_duplicate_loser {
                        let msg = omen_core::message::msg(
                            "plugins.err.duplicate_id",
                            &[&manifest.id, &folder_name],
                        );
                        results.push(InstalledPlugin {
                            id: format!("dup-{}", folder_name),
                            folder_name,
                            manifest: Some(manifest),
                            status: PluginStatus::Error(msg.clone()),
                            enabled: false,
                            can_toggle: false,
                            can_remove: true,
                            diagnostic: Some(msg),
                            pinned_sha256: None,
                        });
                        continue;
                    }

                    let saved = state.plugins.get(&manifest.id);
                    let pinned_sha256 = saved.and_then(|s| s.pinned_sha256.clone());

                    if let Some(filter) = &manifest.vendor_filter
                        && let Err(reason) = filter.check(inventory)
                    {
                        results.push(InstalledPlugin {
                            id: manifest.id.clone(),
                            folder_name,
                            manifest: Some(manifest),
                            status: PluginStatus::Incompatible(reason.clone()),
                            enabled: false,
                            can_toggle: false,
                            can_remove: true,
                            diagnostic: Some(reason),
                            pinned_sha256,
                        });
                        continue;
                    }

                    let enabled = saved.is_some_and(|s| s.enabled);
                    let status = if enabled {
                        PluginStatus::Active
                    } else {
                        PluginStatus::Inactive
                    };
                    results.push(InstalledPlugin {
                        id: manifest.id.clone(),
                        folder_name,
                        manifest: Some(manifest),
                        status,
                        enabled,
                        can_toggle: true,
                        can_remove: true,
                        diagnostic: None,
                        pinned_sha256,
                    });
                }
            }
        }

        results.sort_by(|a, b| a.nav_order().cmp(&b.nav_order()).then(a.id.cmp(&b.id)));
        results
    }

    /// Inspects a `.perfhub-plugin`/`.zip` archive or a `plugin.json` file
    /// without mutating disk state.
    pub fn inspect_package(source: &Path) -> Result<PluginManifest, String> {
        if source
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("plugin.json"))
        {
            return read_and_validate_manifest(source);
        }
        let file = fs::File::open(source).map_err(|e| format!("open plugin package: {e}"))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|e| format!("invalid plugin archive: {e}"))?;
        let (_, manifest) = inspect_zip(&mut archive)?;
        Ok(manifest)
    }

    /// Installs a plugin from a `.perfhub-plugin`/`.zip` archive or a `plugin.json`
    /// file into `%USERPROFILE%\.performance-hub\plugins\<id>\` atomically.
    /// Newly installed plugins always start as `(inaktiv)` (`enabled: false`).
    pub fn install_from_path(
        &self,
        source: &Path,
        replace_existing: bool,
    ) -> Result<InstallOutcome, String> {
        let plugins_dir = self.ensure_plugins_dir()?;
        let is_manifest_file = source
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("plugin.json"));

        let manifest = Self::inspect_package(source)?;
        let target_dir = plugins_dir.join(&manifest.id);
        if target_dir.exists() && !replace_existing {
            return Ok(InstallOutcome::NeedsReplaceConfirm(manifest));
        }

        let nonce = next_nonce();
        let staging_dir = plugins_dir.join(format!(".staging-{nonce}"));
        fs::create_dir_all(&staging_dir).map_err(|e| format!("create staging dir: {e}"))?;

        let stage_res = if is_manifest_file {
            let src_dir = source
                .parent()
                .ok_or_else(|| "plugin.json has no parent folder".to_owned())?;
            copy_dir_bounded(src_dir, &staging_dir)
        } else {
            extract_zip_bounded(source, &staging_dir)
        };
        if let Err(err) = stage_res {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(err);
        }

        // Re-verify manifest inside staged folder before swapping into place.
        if let Err(err) = read_and_validate_manifest(&staging_dir.join("plugin.json")) {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(err);
        }

        let backup_dir = plugins_dir.join(format!(".old-{}-{nonce}", manifest.id));
        let had_existing = target_dir.exists();
        if had_existing && let Err(err) = fs::rename(&target_dir, &backup_dir) {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(omen_core::message::msg(
                "plugins.err.locked",
                &[&err.to_string()],
            ));
        }

        if let Err(err) = fs::rename(&staging_dir, &target_dir) {
            if had_existing {
                let _ = fs::rename(&backup_dir, &target_dir);
            }
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(omen_core::message::msg(
                "plugins.err.locked",
                &[&err.to_string()],
            ));
        }

        if had_existing {
            let _ = fs::remove_dir_all(&backup_dir);
        }

        let mut state = self.load_state().unwrap_or_default();
        state.plugins.insert(
            manifest.id.clone(),
            PluginStateEntry {
                enabled: false,
                pinned_sha256: None,
                installed_at_unix_s: Some(unix_now_s()),
            },
        );
        self.save_state(&state)?;
        Ok(InstallOutcome::Installed(manifest))
    }

    pub fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
        pinned_sha256: Option<String>,
    ) -> Result<(), String> {
        let mut state = self.load_state().unwrap_or_default();
        let entry = state.plugins.entry(id.to_owned()).or_default();
        entry.enabled = enabled;
        if let Some(hash) = pinned_sha256 {
            entry.pinned_sha256 = Some(hash);
        }
        if entry.installed_at_unix_s.is_none() {
            entry.installed_at_unix_s = Some(unix_now_s());
        }
        self.save_state(&state)
    }

    pub fn remove(&self, folder_name: &str, plugin_id: &str) -> Result<(), String> {
        validate_relative_path(folder_name)?;
        if folder_name.contains('/') || folder_name.contains('\\') {
            return Err("invalid plugin folder name".into());
        }
        let plugins_dir = self.plugins_dir();
        let target = plugins_dir.join(folder_name);
        if target.exists() {
            let meta =
                fs::symlink_metadata(&target).map_err(|e| format!("inspect plugin folder: {e}"))?;
            if meta.file_type().is_symlink() {
                return Err("refusing to remove symlinked plugin directory".into());
            }
            let backup = plugins_dir.join(format!(".del-{folder_name}-{}", next_nonce()));
            fs::rename(&target, &backup)
                .map_err(|e| omen_core::message::msg("plugins.err.locked", &[&e.to_string()]))?;
            if let Err(err) = fs::remove_dir_all(&backup) {
                let _ = fs::rename(&backup, &target);
                return Err(omen_core::message::msg(
                    "plugins.err.locked",
                    &[&err.to_string()],
                ));
            }
        }
        let mut state = self.load_state().unwrap_or_default();
        state.plugins.remove(plugin_id);
        self.save_state(&state)
    }
}

fn read_and_validate_manifest(path: &Path) -> Result<PluginManifest, String> {
    let raw = fs::read_to_string(path)
        .map_err(|_| omen_core::message::msg("plugins.err.manifest_missing", &[]))?;
    let manifest: PluginManifest = serde_json::from_str(&raw)
        .map_err(|e| omen_core::message::msg("plugins.err.manifest_invalid", &[&e.to_string()]))?;
    manifest
        .validate()
        .map_err(|e| omen_core::message::msg("plugins.err.manifest_invalid", &[&e]))?;
    Ok(manifest)
}

/// Inspects a zip archive, supporting both root `plugin.json` and single
/// top-level folder `<prefix>/plugin.json`, enforcing all safety limits.
fn inspect_zip<R: Read + io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Result<(Option<String>, PluginManifest), String> {
    if archive.len() > MAX_PLUGIN_FILES * 2 {
        return Err(omen_core::message::msg("plugins.err.too_many_files", &[]));
    }
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut manifest_entries: Vec<(String, String)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("read zip entry: {e}"))?;
        let name = entry.name().replace('\\', "/");
        if entry.is_dir() {
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|m| (m & 0o170000) == 0o120000)
        {
            return Err(omen_core::message::msg("plugins.err.unsafe_archive", &[]));
        }
        validate_relative_path(&name)
            .map_err(|_| omen_core::message::msg("plugins.err.unsafe_archive", &[]))?;
        file_count += 1;
        if file_count > MAX_PLUGIN_FILES {
            return Err(omen_core::message::msg("plugins.err.too_many_files", &[]));
        }
        total_bytes = total_bytes.saturating_add(entry.size());
        if total_bytes > MAX_PLUGIN_UNPACKED_BYTES {
            return Err(omen_core::message::msg(
                "plugins.err.archive_too_large",
                &[],
            ));
        }
        if name == "plugin.json"
            || (name.ends_with("/plugin.json") && name.matches('/').count() == 1)
        {
            let mut text = String::new();
            entry
                .by_ref()
                .take(256 * 1024)
                .read_to_string(&mut text)
                .map_err(|e| format!("read manifest in zip: {e}"))?;
            manifest_entries.push((name, text));
        }
    }
    let (manifest_name, manifest_raw) = manifest_entries
        .into_iter()
        .next()
        .ok_or_else(|| omen_core::message::msg("plugins.err.manifest_missing", &[]))?;
    let strip_prefix = manifest_name
        .strip_suffix("plugin.json")
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    let manifest: PluginManifest = serde_json::from_str(&manifest_raw)
        .map_err(|e| omen_core::message::msg("plugins.err.manifest_invalid", &[&e.to_string()]))?;
    manifest
        .validate()
        .map_err(|e| omen_core::message::msg("plugins.err.manifest_invalid", &[&e]))?;
    Ok((strip_prefix, manifest))
}

fn extract_zip_bounded(source: &Path, dst_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(source).map_err(|e| format!("open plugin archive: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid plugin archive: {e}"))?;
    let (strip_prefix, _) = inspect_zip(&mut archive)?;

    let mut total_written = 0u64;
    let mut file_count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("read zip entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = entry.name().replace('\\', "/");
        let rel = match &strip_prefix {
            Some(prefix) => match raw_name.strip_prefix(prefix) {
                Some(rest) if !rest.is_empty() => rest.to_owned(),
                _ => continue,
            },
            None => raw_name,
        };
        validate_relative_path(&rel)
            .map_err(|_| omen_core::message::msg("plugins.err.unsafe_archive", &[]))?;
        file_count += 1;
        if file_count > MAX_PLUGIN_FILES {
            return Err(omen_core::message::msg("plugins.err.too_many_files", &[]));
        }
        let out_path = dst_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create plugin subdir: {e}"))?;
        }
        let mut out_file =
            fs::File::create(&out_path).map_err(|e| format!("create plugin file: {e}"))?;
        let mut buf = [0u8; 8192];
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| format!("extract plugin file: {e}"))?;
            if n == 0 {
                break;
            }
            total_written = total_written.saturating_add(n as u64);
            if total_written > MAX_PLUGIN_UNPACKED_BYTES {
                return Err(omen_core::message::msg(
                    "plugins.err.archive_too_large",
                    &[],
                ));
            }
            io::Write::write_all(&mut out_file, &buf[..n])
                .map_err(|e| format!("write plugin file: {e}"))?;
        }
    }
    Ok(())
}

fn copy_dir_bounded(src_root: &Path, dst_root: &Path) -> Result<(), String> {
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let cur_src = src_root.join(&rel_dir);
        let cur_dst = dst_root.join(&rel_dir);
        fs::create_dir_all(&cur_dst).map_err(|e| format!("create plugin dir: {e}"))?;
        for entry in fs::read_dir(&cur_src).map_err(|e| format!("read plugin dir: {e}"))? {
            let entry = entry.map_err(|e| format!("read plugin entry: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let src_path = entry.path();
            let meta =
                fs::symlink_metadata(&src_path).map_err(|e| format!("inspect plugin file: {e}"))?;
            if meta.file_type().is_symlink() {
                return Err(omen_core::message::msg("plugins.err.unsafe_archive", &[]));
            }
            let rel_child = rel_dir.join(&name);
            let rel_str = rel_child.to_string_lossy().replace('\\', "/");
            validate_relative_path(&rel_str)
                .map_err(|_| omen_core::message::msg("plugins.err.unsafe_archive", &[]))?;
            if meta.is_dir() {
                stack.push(rel_child);
            } else if meta.is_file() {
                file_count += 1;
                if file_count > MAX_PLUGIN_FILES {
                    return Err(omen_core::message::msg("plugins.err.too_many_files", &[]));
                }
                total_bytes = total_bytes.saturating_add(meta.len());
                if total_bytes > MAX_PLUGIN_UNPACKED_BYTES {
                    return Err(omen_core::message::msg(
                        "plugins.err.archive_too_large",
                        &[],
                    ));
                }
                fs::copy(&src_path, dst_root.join(&rel_child))
                    .map_err(|e| format!("copy plugin file: {e}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("perfhub-plugintest-{tag}-{}", next_nonce()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_lighting_json() -> &'static str {
        r#"{
  "schema": 1,
  "id": "hp-omen-4zone-lighting",
  "name": { "de": "HP OMEN Tastaturbeleuchtung", "en": "HP OMEN Keyboard Lighting" },
  "description": { "de": "4-Zonen-RGB-Steuerung für HP OMEN Notebooks.", "en": "4-zone RGB control for HP OMEN laptops." },
  "version": "2.0.0",
  "api_version": 1,
  "author": "PerformanceHub",
  "category": "lighting",
  "runtime": "host_surface",
  "capabilities": ["hardware.hp_lighting", "ui.sidebar_page"],
  "vendor_filter": { "manufacturers": ["HP"], "require_hp_wmi": true },
  "ui_surface": {
    "kind": "lighting-4zone",
    "nav_label": { "de": "Beleuchtung", "en": "Lighting" },
    "nav_icon": "keyboard",
    "nav_order": 10
  }
}"#
    }

    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn install_toggle_replace_and_remove_zip_plugin() {
        let root = temp_root("lifecycle");
        let store = PluginStore::at(root.clone());
        let pkg = root.join("hp-omen-4zone-lighting.perfhub-plugin");
        write_zip(
            &pkg,
            &[(
                "hp-omen-4zone-lighting/plugin.json",
                sample_lighting_json().as_bytes(),
            )],
        );

        let hp = Inventory {
            manufacturer: "HP".into(),
            hp_bios_interface_present: true,
            ..Default::default()
        };
        let asus = Inventory {
            manufacturer: "ASUS".into(),
            hp_bios_interface_present: false,
            ..Default::default()
        };

        assert!(matches!(
            store.install_from_path(&pkg, false).unwrap(),
            InstallOutcome::Installed(_)
        ));
        let list = store.scan(&hp);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "hp-omen-4zone-lighting");
        assert_eq!(list[0].status, PluginStatus::Inactive);
        assert!(!list[0].enabled);
        assert!(list[0].can_toggle);

        // Enable immediately persists to plugins.json.
        store
            .set_enabled("hp-omen-4zone-lighting", true, None)
            .unwrap();
        let list_active = store.scan(&hp);
        assert_eq!(list_active[0].status, PluginStatus::Active);
        assert!(list_active[0].enabled);

        // On non-HP hardware, plugin reports Incompatible and cannot be toggled.
        let list_non_hp = store.scan(&asus);
        assert!(matches!(
            list_non_hp[0].status,
            PluginStatus::Incompatible(_)
        ));
        assert!(!list_non_hp[0].can_toggle);

        // Re-installing without replace_existing asks for confirmation.
        assert!(matches!(
            store.install_from_path(&pkg, false).unwrap(),
            InstallOutcome::NeedsReplaceConfirm(_)
        ));
        // Confirming replace resets enabled to false.
        assert!(matches!(
            store.install_from_path(&pkg, true).unwrap(),
            InstallOutcome::Installed(_)
        ));
        assert!(!store.scan(&hp)[0].enabled);

        // Remove cleans disk and state.
        store
            .remove("hp-omen-4zone-lighting", "hp-omen-4zone-lighting")
            .unwrap();
        assert!(store.scan(&hp).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zip_slip_archive_is_rejected_without_writing_files() {
        let root = temp_root("zipslip");
        let store = PluginStore::at(root.clone());
        let pkg = root.join("bad.perfhub-plugin");
        write_zip(
            &pkg,
            &[
                ("plugin.json", sample_lighting_json().as_bytes()),
                ("../escaped.txt", b"nope"),
            ],
        );
        assert!(store.install_from_path(&pkg, true).is_err());
        assert!(!root.join("escaped.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_plugin_ids_on_disk_keep_canonical_folder_and_flag_duplicate() {
        let root = temp_root("dup");
        let store = PluginStore::at(root.clone());
        let p1 = store.plugins_dir().join("hp-omen-4zone-lighting");
        let p2 = store.plugins_dir().join("copy-of-lighting");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("plugin.json"), sample_lighting_json()).unwrap();
        fs::write(p2.join("plugin.json"), sample_lighting_json()).unwrap();

        let hp = Inventory {
            manufacturer: "HP".into(),
            hp_bios_interface_present: true,
            ..Default::default()
        };
        let list = store.scan(&hp);
        assert_eq!(list.len(), 2);
        let canonical = list
            .iter()
            .find(|p| p.folder_name == "hp-omen-4zone-lighting")
            .unwrap();
        let dup = list
            .iter()
            .find(|p| p.folder_name == "copy-of-lighting")
            .unwrap();
        assert_eq!(canonical.status, PluginStatus::Inactive);
        assert!(matches!(dup.status, PluginStatus::Error(_)));
        assert!(!dup.can_toggle);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shipped_hp_omen_keyboard_plugin_package_installs_cleanly() {
        let pkg = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/hp-omen-4zone-lighting.perfhub-plugin");
        assert!(
            pkg.exists(),
            "Run scripts/package-plugins.ps1 to build {}",
            pkg.display()
        );
        let root = temp_root("shipped-hp-lighting");
        let store = PluginStore::at(root.clone());
        let outcome = store.install_from_path(&pkg, false).unwrap();
        let InstallOutcome::Installed(manifest) = outcome else {
            panic!("expected fresh install");
        };
        assert_eq!(manifest.id, omen_core::plugins::HP_LIGHTING_PLUGIN_ID);
        assert_eq!(manifest.version, "2.0.0");
        let hp = Inventory {
            manufacturer: "HP".into(),
            hp_bios_interface_present: true,
            ..Default::default()
        };
        let scanned = store.scan(&hp);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].status, PluginStatus::Inactive);
        store.set_enabled(&manifest.id, true, None).unwrap();
        assert_eq!(store.scan(&hp)[0].status, PluginStatus::Active);
        let _ = fs::remove_dir_all(&root);
    }
}
