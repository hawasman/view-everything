//! The Cleaner: Cleanables found from the Index by a Cleanup Rule, reviewed as a Cleanup Plan
//! and removed permanently by Clean (ADR-0008).

use crate::engine::{Repaint, Shared};
use crate::format;
use crate::theme::palette;
use crate::trash::{subtree_on_disk, volume_blocker};
use crate::ui::{caption, panel, pill};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ve_core::{EntryId, Index};
use ve_scan::VolumeInfo;

/// What a `CACHEDIR.TAG` must start with (https://bford.info/cachedir/).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// One thing the Rust build-artefact rule found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanable {
    pub path: PathBuf,
    pub on_disk: u64,
}

/// Rust `target/` folders in the Index confirmed by a sibling `Cargo.toml` or a `CACHEDIR.TAG`
/// inside, on Locations that can be changed, largest first. Nested ones are left out: the
/// outer folder takes them along.
pub fn find_rust_targets(index: &Index, volumes: &[VolumeInfo]) -> Vec<Cleanable> {
    let mut found: Vec<EntryId> = (0..index.capacity() as u32)
        .map(EntryId)
        .filter(|&id| index.is_live(id) && index.is_dir(id) && index.name(id) == "target")
        .filter(|&id| {
            let Some(parent) = index.parent(id) else { return false };
            let is_file = |dir, name| index.child_named(dir, name).is_some_and(|c| !index.is_dir(c));
            is_file(parent, "Cargo.toml") || is_file(id, "CACHEDIR.TAG")
        })
        .filter(|&id| {
            index
                .volume_of(id)
                .and_then(|v| index.volume(v))
                .is_some_and(|v| volume_blocker(v, volumes).is_none())
        })
        .collect();
    let all: HashSet<EntryId> = found.iter().copied().collect();
    found.retain(|&id| {
        let mut up = index.parent(id);
        while let Some(a) = up {
            if all.contains(&a) {
                return false;
            }
            up = index.parent(a);
        }
        true
    });
    let mut out: Vec<Cleanable> = found
        .into_iter()
        .map(|id| Cleanable {
            path: index.path(id),
            on_disk: subtree_on_disk(index, id),
        })
        .collect();
    out.sort_by(|a, b| b.on_disk.cmp(&a.on_disk).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Checks `path` on disk against the rule again: still a real folder (not a link) named
/// `target`, with a sibling `Cargo.toml` or a genuine `CACHEDIR.TAG` inside.
pub fn recheck(path: &Path, volumes: &[VolumeInfo]) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("it is not an absolute path".into());
    }
    // A Volume mounted on or inside it would be emptied too: removal does not stop at mount points.
    if volumes.iter().any(|v| v.mount.starts_with(path)) {
        return Err("a Volume is mounted on or inside it".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| "it no longer exists".to_owned())?;
    if !meta.is_dir() {
        return Err("it is no longer a folder".into());
    }
    if path.file_name().is_none_or(|n| n != "target") {
        return Err("it is not named target".into());
    }
    // Markers must be plain files: a link could point anywhere.
    let plain = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    let cargo = path.parent().is_some_and(|p| plain(&p.join("Cargo.toml")));
    let tag = path.join("CACHEDIR.TAG");
    let tagged = plain(&tag) && std::fs::read(&tag).is_ok_and(|b| b.starts_with(CACHEDIR_SIGNATURE.as_bytes()));
    if cargo || tagged {
        Ok(())
    } else {
        Err("its Cargo.toml and CACHEDIR.TAG marker are gone".into())
    }
}

/// What Clean did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: Vec<PathBuf>,
    /// Paths left alone, each with why.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Rechecks each path and permanently removes the ones that still match. `remove_dir_all` never
/// follows links, so a folder swapped for a link after the recheck loses only the link.
pub fn clean(paths: &[PathBuf], volumes: &[VolumeInfo]) -> Outcome {
    let mut out = Outcome::default();
    for path in paths {
        match recheck(path, volumes).and_then(|()| std::fs::remove_dir_all(path).map_err(|e| e.to_string())) {
            Ok(()) => out.removed.push(path.clone()),
            Err(why) => out.skipped.push((path.clone(), why)),
        }
    }
    out
}

pub struct CleanerPage {
    shared: Arc<Shared>,
    repaint: Repaint,
    volumes: Vec<VolumeInfo>,
    /// Index generation `cleanables` was found in.
    seen: Option<u64>,
    cleanables: Vec<Cleanable>,
    ticked: HashSet<PathBuf>,
    advanced: bool,
}

impl CleanerPage {
    pub fn new(shared: Arc<Shared>, repaint: Repaint, volumes: Vec<VolumeInfo>) -> Self {
        Self {
            shared,
            repaint,
            volumes,
            seen: None,
            cleanables: Vec::new(),
            ticked: HashSet::new(),
            advanced: false,
        }
    }

    pub fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.volumes = volumes;
        self.seen = None;
        cx.notify();
    }

    /// Looks for Cleanables again when the Index changed since the last look.
    // ponytail: scans on the render thread; move to a background thread if a 10M-Entry Index makes it visible.
    fn refresh(&mut self) {
        let index = self.shared.index.read();
        if self.seen == Some(index.generation()) {
            return;
        }
        self.cleanables = find_rust_targets(&index, &self.volumes);
        self.seen = Some(index.generation());
        let present: HashSet<&PathBuf> = self.cleanables.iter().map(|c| &c.path).collect();
        self.ticked.retain(|p| present.contains(p));
    }

    fn plan(&self) -> Vec<&Cleanable> {
        self.cleanables.iter().filter(|c| self.ticked.contains(&c.path)).collect()
    }

    fn ask_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plan: Vec<Cleanable> = self.plan().into_iter().cloned().collect();
        if plan.is_empty() {
            return;
        }
        let total: u64 = plan.iter().map(|c| c.on_disk).sum();
        let this = cx.entity();
        let volumes = self.volumes.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            let paths: Vec<PathBuf> = plan.iter().map(|c| c.path.clone()).collect();
            let volumes = volumes.clone();
            dialog
                .confirm()
                .title("Clean these now?")
                .description(format!(
                    "This permanently removes {} folder(s), freeing about {}. It bypasses the Trash and cannot be undone.",
                    plan.len(),
                    format::size(total)
                ))
                .ok_text("Clean")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |page, cx| page.start_clean(paths.clone(), volumes.clone(), window, cx));
                    true
                })
        });
    }

    fn start_clean(&mut self, paths: Vec<PathBuf>, volumes: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    let outcome = cx.background_executor().spawn(async move { clean(&paths, &volumes) }).await;
                    cx.update(|window, cx| {
                        let (shared, repaint) = {
                            let page = this.read(cx);
                            (page.shared.clone(), page.repaint.clone())
                        };
                        {
                            let mut index = shared.index.write();
                            // Whatever left the disk leaves the Index, skipped-because-vanished included.
                            for path in outcome.removed.iter().chain(
                                outcome
                                    .skipped
                                    .iter()
                                    .filter(|(p, _)| std::fs::symlink_metadata(p).is_err())
                                    .map(|(p, _)| p),
                            ) {
                                if let Some(id) = index.find_path(path) {
                                    index.remove(id);
                                }
                            }
                        }
                        // Wakes the Dashboard and status bar, which read the changed Index.
                        repaint();
                        this.update(cx, |page, cx| {
                            page.ticked.clear();
                            cx.notify();
                        });
                        let mut text = format!("Permanently removed {} folder(s).", outcome.removed.len());
                        if !outcome.skipped.is_empty() {
                            text.push_str(&format!(" Skipped {}: ", outcome.skipped.len()));
                            let why: Vec<String> = outcome.skipped.iter().map(|(p, w)| format!("{} ({w})", p.display())).collect();
                            text.push_str(&why.join("; "));
                        }
                        let note = if outcome.skipped.is_empty() {
                            Notification::info(text)
                        } else {
                            Notification::warning(text)
                        };
                        window.push_notification(note, cx);
                    })
                    .ok();
                }
            })
            .detach();
    }
}

impl Render for CleanerPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh();
        let p = palette(cx);
        let (muted, rule) = (p.muted, p.rule);
        let plan = self.plan();
        let (count, total) = (plan.len(), plan.iter().map(|c| c.on_disk).sum::<u64>());
        let rows = self.cleanables.iter().enumerate().map(|(ix, c)| {
            let path = c.path.clone();
            h_flex()
                .gap(px(12.))
                .py(px(6.))
                .border_b_1()
                .border_color(rule)
                .items_center()
                .child(
                    Checkbox::new(("cleanable", ix))
                        .checked(self.ticked.contains(&c.path))
                        .on_click(cx.listener(move |this, on: &bool, _, cx| {
                            if *on {
                                this.ticked.insert(path.clone());
                            } else {
                                this.ticked.remove(&path);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(c.path.display().to_string()))
                        .child(caption("Rust build artefacts", cx)),
                )
                .child(div().flex_none().child(format::size(c.on_disk)))
        });
        let advanced = self.advanced.then(|| {
            v_flex()
                .gap_1()
                .mt(px(12.))
                .p_2()
                .rounded_md()
                .bg(cx.theme().muted)
                .font_family(cx.theme().mono_font_family.clone())
                .child(caption("Removed permanently, exactly these paths:", cx))
                .children(plan.iter().map(|c| div().child(c.path.display().to_string())))
                .when(count == 0, |d| d.child(caption("Nothing ticked.", cx)))
        });
        div().id("cleaner").size_full().overflow_y_scroll().child(
            v_flex()
                .gap(px(16.))
                .p(px(16.))
                .child(div().text_size(px(24.)).line_height(px(32.)).child("Cleaner"))
                .child(
                    div()
                        .text_color(muted)
                        .child("Rust target folders, found in the Index: a sibling Cargo.toml or a CACHEDIR.TAG inside confirms them."),
                )
                .child(
                    panel(cx)
                        .child(v_flex().children(rows))
                        .when(self.cleanables.is_empty(), |d| d.child("Nothing to clean."))
                        .child(
                            h_flex()
                                .mt(px(12.))
                                .gap(px(8.))
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(format!("Cleanup Plan: {count} selected, {}", format::size(total))),
                                )
                                .child(pill("plan-advanced", "Advanced").small().selected(self.advanced).build(
                                    cx,
                                    cx.listener(|this, _, _, cx| {
                                        this.advanced = !this.advanced;
                                        cx.notify();
                                    }),
                                ))
                                .child(
                                    pill("clean", "Clean")
                                        .primary()
                                        .disabled((count == 0).then_some("Tick what to clean first"))
                                        .build(cx, cx.listener(|this, _, window, cx| this.ask_clean(window, cx))),
                                ),
                        )
                        .children(advanced),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob would bring gpui's own `test` attribute into scope.
    use super::{CACHEDIR_SIGNATURE, Index, Path, PathBuf, VolumeInfo, clean, find_rust_targets, recheck};
    use ve_core::{EntryMeta, SyncState};

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    fn file(size: u64) -> EntryMeta {
        EntryMeta {
            on_disk: size,
            ..Default::default()
        }
    }

    #[test]
    fn finds_only_marked_targets_on_writable_locations() {
        let mut index = Index::new();
        let vol = index.add_volume(Path::new("/w"), "ext4", "uuid:w", dir(), SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        // Sibling Cargo.toml.
        let a = index.insert(root, "a", dir());
        index.insert(a, "Cargo.toml", file(10));
        let target = index.insert(a, "target", dir());
        index.insert(target, "big", file(4096));
        // A nested target inside it is not listed separately.
        let inner = index.insert(target, "target", dir());
        index.insert(inner, "CACHEDIR.TAG", file(1));
        // Marker inside, no Cargo.toml.
        let b = index.insert(root, "b", dir());
        let tagged = index.insert(b, "target", dir());
        index.insert(tagged, "CACHEDIR.TAG", file(8));
        // Unmarked `target`, and a Cargo.toml *folder* doesn't count.
        let c = index.insert(root, "c", dir());
        index.insert(c, "target", dir());
        let d = index.insert(root, "d", dir());
        index.insert(d, "Cargo.toml", dir());
        index.insert(d, "target", dir());

        let found = find_rust_targets(&index, &[]);
        let paths: Vec<_> = found.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/w/a/target"), PathBuf::from("/w/b/target")]);
        assert_eq!(found[0].on_disk, 4096 + 1);

        // Read-only mounts, Offline Volumes and Remotes are never offered.
        let ro = VolumeInfo {
            mount: "/w".into(),
            identity: "uuid:w".into(),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: false,
            network: false,
            read_only: true,
        };
        assert!(find_rust_targets(&index, &[ro]).is_empty());
        index.set_mount(vol, None);
        assert!(find_rust_targets(&index, &[]).is_empty());
        let remote = index.add_remote(&ve_core::Remote::new("me@box", "/srv").unwrap());
        let rroot = index.volume(remote).unwrap().root;
        let r = index.insert(rroot, "p", dir());
        index.insert(r, "Cargo.toml", file(1));
        index.insert(r, "target", dir());
        assert!(find_rust_targets(&index, &[]).is_empty());
    }

    #[test]
    fn clean_rechecks_on_disk() {
        let base = std::env::temp_dir().join(format!("ve-cleaner-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let make = |name: &str, cargo: bool, tag: Option<&str>| {
            let target = base.join(name).join("target");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("artefact"), "x").unwrap();
            if cargo {
                std::fs::write(base.join(name).join("Cargo.toml"), "").unwrap();
            }
            if let Some(tag) = tag {
                std::fs::write(target.join("CACHEDIR.TAG"), tag).unwrap();
            }
            target
        };
        let cargo = make("cargo", true, None);
        let tagged = make("tagged", false, Some(CACHEDIR_SIGNATURE));
        let fake_tag = make("faketag", false, Some("nope"));
        let unmarked = make("unmarked", false, None);
        let vanished = base.join("vanished").join("target");

        let out = clean(
            &[cargo.clone(), tagged.clone(), fake_tag.clone(), unmarked.clone(), vanished.clone()],
            &[],
        );
        assert_eq!(out.removed, vec![cargo.clone(), tagged.clone()]);
        assert!(!cargo.exists() && !tagged.exists());
        assert!(fake_tag.exists() && unmarked.exists());
        assert_eq!(out.skipped.len(), 3);
        assert!(out.skipped.iter().any(|(p, _)| *p == vanished));
        // A marker that vanished since the Index was built skips the folder too.
        std::fs::remove_file(fake_tag.join("CACHEDIR.TAG")).unwrap();
        assert!(recheck(&fake_tag, &[]).is_err());
        // A symlinked Cargo.toml is no marker, and a symlinked target is never removed.
        #[cfg(unix)]
        {
            let linked = make("linked", false, None);
            std::os::unix::fs::symlink(base.join("unmarked").join("target"), base.join("linked").join("Cargo.toml")).unwrap();
            assert!(recheck(&linked, &[]).is_err());
            let alias = base.join("alias").join("target");
            std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
            std::fs::write(alias.parent().unwrap().join("Cargo.toml"), "").unwrap();
            std::os::unix::fs::symlink(&unmarked, &alias).unwrap();
            assert!(recheck(&alias, &[]).is_err());
        }
        // A Volume mounted inside, a relative path and a symlinked CACHEDIR.TAG are all refused.
        let mounted = make("mounted", true, None);
        let inside = VolumeInfo {
            mount: mounted.join("deep"),
            identity: "uuid:m".into(),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: true,
            network: false,
            read_only: false,
        };
        assert!(recheck(&mounted, &[inside]).is_err());
        assert!(recheck(Path::new("target"), &[]).is_err());
        #[cfg(unix)]
        {
            let tagged_link = make("taglink", false, None);
            let real = base.join("real.tag");
            std::fs::write(&real, CACHEDIR_SIGNATURE).unwrap();
            std::os::unix::fs::symlink(&real, tagged_link.join("CACHEDIR.TAG")).unwrap();
            assert!(recheck(&tagged_link, &[]).is_err());
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
