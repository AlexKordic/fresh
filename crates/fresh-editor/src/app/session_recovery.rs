//! Import FC 0.2.3 checkpoints into the native workspace/recovery store.
//! The caller retains the original until native persistence succeeds.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Editor;
use crate::model::event::BufferId;
use crate::workspace::Workspace;

#[derive(Serialize, Deserialize)]
struct SavedBuffer {
    id: BufferId,
    path: Option<PathBuf>,
    name: String,
    text: Option<String>,
    modified: bool,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct SavedView {
    tabs: Vec<BufferId>,
    active: BufferId,
    cursor: usize,
    anchor: Option<usize>,
    top_byte: usize,
    left_column: usize,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    workspace: Workspace,
    buffers: Vec<SavedBuffer>,
    // Leaf order is preserved by workspace split-layout restoration.
    views: Vec<SavedView>,
}

impl Editor {
    pub fn session_has_modified_buffers(&self) -> bool {
        self.windows
            .values()
            .any(|w| w.buffers.iter().any(|(_, s)| s.buffer.is_modified()))
    }

    /// Persist recovery before tab membership: unnamed tabs need their recovery
    /// IDs allocated before the workspace is captured. Never writes source files.
    pub fn checkpoint_session(&mut self) -> anyhow::Result<()> {
        for id in self.window_ids_sorted() {
            self.with_window_retargeted(id, |editor| editor.save_pending_recovery_buffers())?;
        }
        self.save_all_windows_workspaces()?;
        Ok(())
    }

    /// A user-requested backend restart must not fall through to best-effort
    /// exit persistence or auto-save source files. Refuse before stopping if
    /// the next backend cannot recover the current tabs and unsaved text.
    pub fn checkpoint_for_restart(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.workspace_persistence_enabled,
            "Enable workspace persistence before restarting the editor"
        );
        anyhow::ensure!(
            self.config().editor.restore_previous_session,
            "Enable restore_previous_session before restarting the editor"
        );
        anyhow::ensure!(
            self.recovery_service.lock().unwrap().is_enabled(),
            "Enable recovery before restarting the editor"
        );
        for id in self.window_ids_sorted() {
            self.with_window_retargeted(id, |editor| {
                // Re-write every unsaved snapshot, including empty untitled
                // tabs and buffers whose previous recovery write is missing.
                let ids: Vec<_> = editor
                    .buffers()
                    .iter()
                    .filter_map(|(id, state)| {
                        let meta = editor.active_window().buffer_metadata.get(id)?;
                        if meta.hidden_from_tabs || meta.is_virtual() || state.is_composite_buffer {
                            return None;
                        }
                        (state.buffer.is_modified()
                            || meta.file_path().is_none_or(|p| p.as_os_str().is_empty()))
                        .then_some(*id)
                    })
                    .collect();
                for id in ids {
                    editor
                        .buffers_mut()
                        .get_mut(&id)
                        .unwrap()
                        .buffer
                        .set_recovery_pending(true);
                }
            });
        }
        self.checkpoint_session()
    }

    pub fn restore_session_checkpoint(&mut self, path: &Path) -> anyhow::Result<bool> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let checkpoint: Checkpoint = serde_json::from_reader(file)?;
        anyhow::ensure!(
            checkpoint.version == 1,
            "Unsupported session checkpoint version"
        );
        if checkpoint.workspace.working_dir.is_dir() {
            self.active_window_mut().root = checkpoint.workspace.working_dir.clone();
        }
        anyhow::ensure!(
            self.recovery_service.lock().unwrap().is_enabled(),
            "Enable Fresh recovery before importing an FC checkpoint"
        );
        let session = self.session_name.clone();
        self.active_window_mut()
            .apply_workspace_layout(&checkpoint.workspace, session.as_deref());
        let mut ids = HashMap::new();
        for saved in checkpoint.buffers {
            let id = if let Some(ref path) = saved.path {
                // Opening a now-missing file keeps its intended path; dirty text
                // remains available without silently writing it to the filesystem.
                self.open_file(path)?
            } else {
                self.new_buffer()
            };
            let state = self.buffers_mut().get_mut(&id).unwrap();
            if let Some(text) = saved.text {
                state.buffer.replace_content(&text);
                state.buffer.set_modified(saved.modified);
                state.buffer.set_recovery_pending(true);
                state.wrap_indices.damage_all();
            }
            let len = state.buffer.total_bytes();
            for view in self
                .active_window_mut()
                .split_view_states_mut()
                .values_mut()
            {
                let state = view.ensure_buffer_state(id);
                state.cursors.primary_mut().position = saved.cursor.min(len);
                state.cursors.primary_mut().anchor = saved.anchor.map(|v| v.min(len));
            }
            if let Some(meta) = self.active_window_mut().buffer_metadata.get_mut(&id) {
                meta.display_name = saved.name;
            }
            if saved.modified {
                self.active_window_mut()
                    .event_logs
                    .get_mut(&id)
                    .unwrap()
                    .clear_saved_position();
                self.sync_lsp_after_recovery_replay(id);
            }
            ids.insert(saved.id, id);
        }
        let leaves = self.active_window().split_manager().root().visible_leaves();
        for ((split, _), saved) in leaves.into_iter().zip(checkpoint.views) {
            let tabs: Vec<_> = saved
                .tabs
                .iter()
                .filter_map(|id| ids.get(id).copied())
                .collect();
            // Virtual/terminal-only views are restored by the normal workspace.
            if tabs.is_empty() {
                continue;
            }
            let active = ids.get(&saved.active).copied().unwrap_or(tabs[0]);
            let len = self.buffers().get(&active).unwrap().buffer.total_bytes();
            let Some(view) = self
                .active_window_mut()
                .split_view_states_mut()
                .get_mut(&split)
            else {
                continue;
            };
            view.open_buffers = tabs
                .into_iter()
                .map(crate::view::split::TabTarget::Buffer)
                .collect();
            view.focus_history.clear();
            let view = view.ensure_buffer_state(active);
            view.cursors.primary_mut().position = saved.cursor.min(len);
            view.cursors.primary_mut().anchor = saved.anchor.map(|v| v.min(len));
            view.viewport.set_top_byte(saved.top_byte.min(len));
            view.viewport.left_column = saved.left_column;
            view.viewport.set_skip_resize_sync();
            self.active_window_mut()
                .split_manager_mut()
                .set_split_buffer(split, active);
        }
        Ok(true)
    }
}
