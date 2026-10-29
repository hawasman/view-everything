//! The Settings page: which Volumes are indexed, how often they are re-read, and the theme.

use crate::engine::{Intervals, last_synced_label};
use crate::settings::{Settings, ThemeChoice};
use crate::theme::{self, SERIF_FAMILY, palette};
use crate::ui::{Tone, caption, panel, pill, tag};
use gpui_kit::component::IndexPath;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;
use std::path::PathBuf;
use ve_core::{AzureRemote, FtpRemote, FtpTls, Remote, S3Remote, WebdavRemote};
use ve_scan::VolumeInfo;

/// The protocol tabs of the Add Remote dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteTab {
    Ssh,
    S3,
    Ftp,
    Webdav,
    Azure,
}

/// The refresh choices, in minutes; 0 is Manual (never re-read while running).
const REFRESH_CHOICES: [u64; 6] = [15, 30, 60, 120, 360, 0];

#[derive(Clone)]
struct Every(u64);

impl SelectItem for Every {
    type Value = u64;
    fn value(&self) -> &u64 {
        &self.0
    }
    fn title(&self) -> SharedString {
        match self.0 {
            0 => "Manual".into(),
            m if m % 60 == 0 => format!("{} h", m / 60).into(),
            m => format!("{m} min").into(),
        }
    }
}

type RefreshSelect = Entity<SelectState<Vec<Every>>>;

/// The refresh choices, plus `minutes` if it is not one of them, and where `minutes` is.
fn refresh_items(minutes: u64) -> (Vec<Every>, IndexPath) {
    let mut items: Vec<Every> = REFRESH_CHOICES.map(Every).into();
    if !REFRESH_CHOICES.contains(&minutes) {
        items.insert(items.len() - 1, Every(minutes));
    }
    let at = items.iter().position(|e| e.0 == minutes).expect("minutes is among the items");
    (items, IndexPath::new(at))
}

/// The age limit choices of the Cleaner, in days.
const DAY_CHOICES: [u64; 7] = [1, 3, 7, 14, 30, 60, 90];

#[derive(Clone)]
struct Days(u64);

impl SelectItem for Days {
    type Value = u64;
    fn value(&self) -> &u64 {
        &self.0
    }
    fn title(&self) -> SharedString {
        match self.0 {
            1 => "1 day".into(),
            d => format!("{d} days").into(),
        }
    }
}

type DaysSelect = Entity<SelectState<Vec<Days>>>;

/// The fields of the Add Remote dialog's S3 tab.
#[derive(Clone)]
struct S3Form {
    bucket: Entity<InputState>,
    prefix: Entity<InputState>,
    endpoint: Entity<InputState>,
    region: Entity<InputState>,
    profile: Entity<InputState>,
    key: Entity<InputState>,
    secret: Entity<InputState>,
}

impl S3Form {
    fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        let mut input = |hint: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let (bucket, prefix, endpoint) = (
            input("my-bucket"),
            input("backups/2024 (optional)"),
            input("https://host:9000 (optional)"),
        );
        let (region, profile, key) = (input("us-east-1"), input("default"), input("AKIA..."));
        let secret = cx.new(|cx| InputState::new(window, cx).placeholder("stored in the OS keyring").masked(true));
        Self {
            bucket,
            prefix,
            endpoint,
            region,
            profile,
            key,
            secret,
        }
    }

    fn all(&self) -> [&Entity<InputState>; 7] {
        [
            &self.bucket,
            &self.prefix,
            &self.endpoint,
            &self.region,
            &self.profile,
            &self.key,
            &self.secret,
        ]
    }

    /// The S3 Remote typed in and the secret key typed for it, if any. Empty Region and Endpoint
    /// are taken from the profile or the environment, as the AWS tools do. `editing` is the Remote
    /// the dialog edits: its secret stays when none is typed for the same Access key.
    fn read(&self, path_style: bool, editing: Option<&Remote>, cx: &App) -> Result<(Remote, Option<String>), String> {
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_owned();
        let (profile, key, secret) = (text(&self.profile), text(&self.key), self.secret.read(cx).value().to_string());
        let defaults = ve_scan::s3::with_system_chain(|c| c.defaults(Some(profile.as_str()).filter(|p| !p.is_empty())));
        let (mut region, mut endpoint) = (text(&self.region), text(&self.endpoint));
        if region.is_empty() {
            region = defaults.region.unwrap_or_else(|| "us-east-1".into());
        }
        if endpoint.is_empty() {
            endpoint = defaults.endpoint.unwrap_or_default();
        }
        let remote = S3Remote::new(
            &text(&self.bucket),
            &text(&self.prefix),
            &endpoint,
            &region,
            path_style,
            &profile,
            &key,
        )?;
        let kept = matches!(editing, Some(Remote::S3(old)) if old.access_key.is_some() && old.access_key == remote.access_key);
        match (key.is_empty(), secret.is_empty()) {
            (false, _) if !profile.is_empty() => return Err("Use a Profile or an Access key, not both".into()),
            (false, true) if kept => {}
            (false, true) => return Err("Enter the Secret key that goes with the Access key".into()),
            (true, false) => return Err("Enter the Access key that goes with the Secret key".into()),
            _ => {}
        }
        Ok((Remote::S3(remote), Some(secret).filter(|s| !s.is_empty())))
    }

    /// Shows `remote` in the fields; its secret is not shown, only said to be kept.
    fn fill(&self, remote: &S3Remote, window: &mut Window, cx: &mut App) {
        set(&self.bucket, &remote.bucket, window, cx);
        set(&self.prefix, &remote.prefix, window, cx);
        set(&self.endpoint, remote.endpoint.as_deref().unwrap_or(""), window, cx);
        set(&self.region, &remote.region, window, cx);
        set(&self.profile, remote.profile.as_deref().unwrap_or(""), window, cx);
        set(&self.key, remote.access_key.as_deref().unwrap_or(""), window, cx);
        set(&self.secret, "", window, cx);
        secret_hint(&self.secret, remote.access_key.is_some(), window, cx);
    }
}

fn set(input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |i, cx| i.set_value(text, window, cx));
}

/// The placeholder of a secret field: kept already (when editing a Remote that has one), or to be kept.
fn secret_hint(input: &Entity<InputState>, kept: bool, window: &mut Window, cx: &mut App) {
    let hint = if kept {
        "unchanged (in the OS keyring)"
    } else {
        "stored in the OS keyring"
    };
    input.update(cx, |i, cx| i.set_placeholder(hint, window, cx));
}

/// The fields of the Add Remote dialog's FTP tab.
#[derive(Clone)]
struct FtpForm {
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    path: Entity<InputState>,
}

impl FtpForm {
    fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        let mut input = |hint: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let (host, port, user, path) = (input("ftp.example.com"), input("21"), input("anonymous"), input("/pub (optional)"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("stored in the OS keyring").masked(true));
        Self {
            host,
            port,
            user,
            password,
            path,
        }
    }

    fn all(&self) -> [&Entity<InputState>; 5] {
        [&self.host, &self.port, &self.user, &self.password, &self.path]
    }

    /// The FTP Remote typed in and the password typed for it, if any. `editing` is the Remote the
    /// dialog edits: its password stays when none is typed.
    fn read(&self, allow_plain: bool, editing: Option<&Remote>, cx: &App) -> Result<(Remote, Option<String>), String> {
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_owned();
        let port = match text(&self.port).as_str() {
            "" => 21,
            p => p.parse().map_err(|_| "Port must be a number, usually 21".to_owned())?,
        };
        let tls = if allow_plain { FtpTls::Plain } else { FtpTls::Explicit };
        let remote = FtpRemote::new(&text(&self.host), port, &text(&self.user), &text(&self.path), tls)?;
        let password = self.password.read(cx).value().to_string();
        let kept = matches!(editing, Some(Remote::Ftp(old)) if !old.is_anonymous());
        if password.is_empty() && !remote.is_anonymous() && !kept {
            return Err("Enter the password for this user".into());
        }
        Ok((Remote::Ftp(remote), Some(password).filter(|p| !p.is_empty())))
    }

    /// Shows `remote` in the fields; its password is not shown, only said to be kept.
    fn fill(&self, remote: &FtpRemote, window: &mut Window, cx: &mut App) {
        set(&self.host, &remote.host, window, cx);
        set(&self.port, &remote.port.to_string(), window, cx);
        set(&self.user, &remote.user, window, cx);
        set(&self.password, "", window, cx);
        set(&self.path, &remote.path, window, cx);
        secret_hint(&self.password, !remote.is_anonymous(), window, cx);
    }
}

/// The fields of the Add Remote dialog's WebDAV tab.
#[derive(Clone)]
struct WebdavForm {
    url: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
}

impl WebdavForm {
    fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        let mut input = |hint: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let (url, user) = (input("https://host/remote.php/dav/files/me"), input("empty for none"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("stored in the OS keyring").masked(true));
        Self { url, user, password }
    }

    fn all(&self) -> [&Entity<InputState>; 3] {
        [&self.url, &self.user, &self.password]
    }

    /// The WebDAV Remote typed in and the password typed for it, if any. `editing` is the Remote
    /// the dialog edits: its password stays when none is typed and the server and user are unchanged.
    fn read(&self, editing: Option<&Remote>, cx: &App) -> Result<(Remote, Option<String>), String> {
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_owned();
        let remote = WebdavRemote::new(&text(&self.url), &text(&self.user))?;
        let password = self.password.read(cx).value().to_string();
        // The stored password is only for the server and user it was typed for.
        let kept = matches!(editing, Some(Remote::Webdav(old)) if !old.user.is_empty() && *old == remote);
        if password.is_empty() && !remote.user.is_empty() && !kept {
            return Err("Enter the password for this user".into());
        }
        let password = Some(password).filter(|p| !p.is_empty() && !remote.user.is_empty());
        Ok((Remote::Webdav(remote), password))
    }

    /// Shows `remote` in the fields; its password is not shown, only said to be kept.
    fn fill(&self, remote: &WebdavRemote, window: &mut Window, cx: &mut App) {
        set(&self.url, &remote.url, window, cx);
        set(&self.user, &remote.user, window, cx);
        set(&self.password, "", window, cx);
        secret_hint(&self.password, !remote.user.is_empty(), window, cx);
    }
}

/// The fields of the Add Remote dialog's Azure Blob tab.
#[derive(Clone)]
struct AzureForm {
    account: Entity<InputState>,
    container: Entity<InputState>,
    prefix: Entity<InputState>,
    endpoint: Entity<InputState>,
    secret: Entity<InputState>,
}

impl AzureForm {
    fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        let mut input = |hint: &'static str| cx.new(|cx| InputState::new(window, cx).placeholder(hint));
        let (account, container, prefix) = (input("mystorageaccount"), input("backups"), input("2024 (optional)"));
        let endpoint = input("Azurite etc. (optional)");
        let secret = cx.new(|cx| InputState::new(window, cx).placeholder("stored in the OS keyring").masked(true));
        Self {
            account,
            container,
            prefix,
            endpoint,
            secret,
        }
    }

    fn all(&self) -> [&Entity<InputState>; 5] {
        [&self.account, &self.container, &self.prefix, &self.endpoint, &self.secret]
    }

    /// The Azure Blob Remote typed in and the account key or SAS token typed for it, if any.
    /// `editing` is the Remote the dialog edits: its secret stays when none is typed, unless it
    /// was the other kind of secret or is for another endpoint.
    fn read(&self, sas: bool, editing: Option<&Remote>, cx: &App) -> Result<(Remote, Option<String>), String> {
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_owned();
        let remote = AzureRemote::new(
            &text(&self.account),
            &text(&self.container),
            &text(&self.prefix),
            &text(&self.endpoint),
            sas,
        )?;
        let secret = self.secret.read(cx).value().trim().to_owned();
        // The stored secret is only for the endpoint and kind it was typed for.
        let kept = matches!(editing, Some(Remote::Azure(old)) if old.identity() == remote.identity() && old.sas == sas);
        if secret.is_empty() && !kept {
            return Err(if sas { "Enter the SAS token" } else { "Enter the account key" }.into());
        }
        Ok((Remote::Azure(remote), Some(secret).filter(|s| !s.is_empty())))
    }

    /// Shows `remote` in the fields; its secret is not shown, only said to be kept.
    fn fill(&self, remote: &AzureRemote, window: &mut Window, cx: &mut App) {
        set(&self.account, &remote.account, window, cx);
        set(&self.container, &remote.container, window, cx);
        set(&self.prefix, &remote.prefix, window, cx);
        set(&self.endpoint, remote.endpoint.as_deref().unwrap_or(""), window, cx);
        set(&self.secret, "", window, cx);
        secret_hint(&self.secret, true, window, cx);
    }
}

/// What the Engine must be told after the user changes a setting.
pub struct Changed {
    pub mounted: Vec<VolumeInfo>,
    pub indexed: Vec<VolumeInfo>,
    pub remotes: Vec<Remote>,
    pub intervals: Intervals,
}

/// The user asked to Refresh the Remote with this identity now.
pub struct Refresh(pub String);

/// The user asked to check for a newer version now.
#[cfg(feature = "updater")]
pub struct CheckNow;

/// The user asked to Forget the Offline Volume with this identity.
pub struct Forget(pub String);

pub struct SettingsPage {
    settings: Settings,
    /// Offline Volumes as (identity, last mount path).
    offline: Vec<(String, PathBuf)>,
    volumes: Vec<VolumeInfo>,
    refresh: RefreshSelect,
    /// One refresh-interval select per Volume, in `volumes` order.
    volume_refresh: Vec<RefreshSelect>,
    /// The Cleaner's age limits for pre-ticking temporary files and build artefacts.
    temp_days: DaysSelect,
    build_days: DaysSelect,
    /// Set when the settings file could not be written.
    problem: Option<String>,
    /// Fields of the Remote dialog.
    remote_host: Entity<InputState>,
    remote_path: Entity<InputState>,
    remote_every: RefreshSelect,
    s3: S3Form,
    ftp: FtpForm,
    webdav: WebdavForm,
    azure: AzureForm,
    /// The tab the Remote dialog shows.
    remote_tab: RemoteTab,
    s3_path_style: bool,
    /// Azure Blob's secret is a SAS token rather than an account key.
    azure_sas: bool,
    /// FTP without TLS was switched on in the dialog.
    ftp_plain: bool,
    /// The Remote the dialog edits; `None` adds one.
    editing: Option<Remote>,
    /// What the dialog's Test found for the details typed in.
    check: Check,
    /// Counts Tests and changes to the details, so a Test that ends after either is not shown.
    check_run: u64,
    /// One refresh-interval select per Remote, in `settings.remotes` order.
    remote_refresh: Vec<RefreshSelect>,
    /// Last Synced of each Remote by identity, as the Engine reports it.
    remote_synced: HashMap<String, Option<i64>>,
    /// What the last update check found, for the Updates section.
    #[cfg(feature = "updater")]
    update_status: String,
}

/// The Test of the details in the Remote dialog.
#[derive(Clone, PartialEq)]
enum Check {
    Idle,
    Running,
    Passed,
    Failed(String),
}

impl EventEmitter<Changed> for SettingsPage {}
impl EventEmitter<Forget> for SettingsPage {}
impl EventEmitter<Refresh> for SettingsPage {}
#[cfg(feature = "updater")]
impl EventEmitter<CheckNow> for SettingsPage {}

impl SettingsPage {
    pub fn new(settings: Settings, volumes: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let refresh = Self::refresh_select(settings.refresh_minutes, None, window, cx);
        let volume_refresh = volumes
            .iter()
            .map(|v| Self::refresh_select(settings.refresh_minutes_for(v), Some(v.identity.clone()), window, cx))
            .collect();
        let temp_days = Self::days_select(settings.cleaner_temp_days, false, window, cx);
        let build_days = Self::days_select(settings.cleaner_build_days, true, window, cx);
        let remote_host = cx.new(|cx| InputState::new(window, cx).placeholder("user@host or ssh config alias"));
        let remote_path = cx.new(|cx| InputState::new(window, cx).placeholder("/srv/backup"));
        let (items, manual) = refresh_items(0);
        let remote_every = cx.new(|cx| SelectState::new(items, Some(manual), window, cx));
        let remote_refresh = Self::remote_selects(&settings, window, cx);
        let (s3, ftp) = (S3Form::new(window, cx), FtpForm::new(window, cx));
        let (webdav, azure) = (WebdavForm::new(window, cx), AzureForm::new(window, cx));
        // A Test result is for the details it tested.
        for input in [&remote_host, &remote_path]
            .into_iter()
            .chain(s3.all())
            .chain(ftp.all())
            .chain(webdav.all())
            .chain(azure.all())
        {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.forget_check(cx);
                }
            })
            .detach();
        }
        Self {
            settings,
            offline: Vec::new(),
            volumes,
            refresh,
            volume_refresh,
            temp_days,
            build_days,
            problem: None,
            remote_host,
            remote_path,
            remote_every,
            s3,
            ftp,
            webdav,
            azure,
            // Screenshots: VE_DIALOG_TAB=s3, ftp, webdav or azure opens the Add Remote dialog on that tab, VE_DIALOG_PLAIN
            // with FTP's plain toggle on.
            remote_tab: match (cfg!(feature = "bench"), std::env::var("VE_DIALOG_TAB").as_deref()) {
                (true, Ok("s3")) => RemoteTab::S3,
                (true, Ok("ftp")) => RemoteTab::Ftp,
                (true, Ok("webdav")) => RemoteTab::Webdav,
                (true, Ok("azure")) => RemoteTab::Azure,
                _ => RemoteTab::Ssh,
            },
            s3_path_style: false,
            azure_sas: false,
            ftp_plain: cfg!(feature = "bench") && std::env::var_os("VE_DIALOG_PLAIN").is_some(),
            editing: None,
            check: Check::Idle,
            check_run: 0,
            remote_refresh,
            remote_synced: HashMap::new(),
            #[cfg(feature = "updater")]
            update_status: String::new(),
        }
    }

    fn remote_selects(settings: &Settings, window: &mut Window, cx: &mut Context<Self>) -> Vec<RefreshSelect> {
        settings
            .remotes
            .iter()
            .map(|r| Self::refresh_select(settings.remote_refresh_minutes(r), Some(r.identity()), window, cx))
            .collect()
    }

    /// The Remote the dialog describes, with the secret typed for it (none typed: `None`).
    fn typed_remote(&self, cx: &App) -> Result<(Remote, Option<String>), String> {
        match self.remote_tab {
            RemoteTab::S3 => self.s3.read(self.s3_path_style, self.editing.as_ref(), cx),
            RemoteTab::Ftp => self.ftp.read(self.ftp_plain, self.editing.as_ref(), cx),
            RemoteTab::Webdav => self.webdav.read(self.editing.as_ref(), cx),
            RemoteTab::Azure => self.azure.read(self.azure_sas, self.editing.as_ref(), cx),
            RemoteTab::Ssh => Ok((
                Remote::new(
                    self.remote_host.read(cx).value().as_ref(),
                    self.remote_path.read(cx).value().as_ref(),
                )?,
                None,
            )),
        }
    }

    /// Adds the Remote, or puts it in place of the one being edited, with its refresh interval,
    /// and has it indexed. A typed secret is kept under the Remote's identity; with none typed, an
    /// edited Remote's secret moves with it to a new identity.
    fn save_remote(
        &mut self,
        remote: Remote,
        secret: Option<String>,
        minutes: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let id = remote.identity();
        let old = self.editing.clone();
        let moved = old.as_ref().filter(|o| o.has_secret() && o.identity() != id);
        match (&secret, moved) {
            (Some(secret), _) => ve_scan::remote::store_secret(&id, secret)?,
            (None, Some(o)) if remote.has_secret() => match ve_scan::remote::read_secret(&o.identity()) {
                Ok(secret) => ve_scan::remote::store_secret(&id, &secret)?,
                Err(why) if remote.needs_secret() => return Err(format!("Enter the secret again: {why}")),
                Err(_) => {}
            },
            _ => {}
        }
        if let Some(o) = &old
            && o.has_secret()
            && (o.identity() != id || !remote.has_secret())
        {
            ve_scan::remote::forget_secret(o);
        }
        match &old {
            Some(o) => self.settings.replace_remote(o, remote, minutes),
            None => {
                self.settings.add_remote(remote, minutes);
            }
        }
        self.editing = None;
        self.remote_refresh = Self::remote_selects(&self.settings, window, cx);
        self.commit(cx);
        Ok(())
    }

    /// Tests the details typed in, off the UI thread, and shows what it found in the dialog.
    fn test_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_run += 1;
        let run = self.check_run;
        let (remote, secret) = match self.typed_remote(cx) {
            Ok(typed) => typed,
            Err(why) => {
                self.check = Check::Failed(why);
                window.refresh();
                return;
            }
        };
        self.check = Check::Running;
        window.refresh();
        // With none typed, an edited Remote's secret is still filed under its old identity until saved.
        let kept = self.editing.as_ref().filter(|o| o.has_secret()).map(Remote::identity);
        let task = cx.background_spawn(async move {
            let secret = secret.or_else(|| kept.and_then(|id| ve_scan::remote::read_secret(&id).ok()));
            ve_scan::remote::check(&remote, secret.as_deref())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |page, window, _| {
                if page.check_run == run {
                    page.check = match result {
                        Ok(()) => Check::Passed,
                        Err(e) => Check::Failed(e.to_string()),
                    };
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn forget_check(&mut self, cx: &mut Context<Self>) {
        self.check_run += 1;
        if self.check != Check::Idle {
            self.check = Check::Idle;
            cx.notify();
        }
    }

    fn remove_remote(&mut self, remote: &Remote, window: &mut Window, cx: &mut Context<Self>) {
        ve_scan::remote::forget_secret(remote);
        self.settings.remove_remote(remote);
        self.remote_refresh = Self::remote_selects(&self.settings, window, cx);
        self.commit(cx);
    }

    /// Replaces the Last Synced of each Remote (by identity) the Engine reports.
    pub fn set_remote_synced(&mut self, synced: HashMap<String, Option<i64>>, cx: &mut Context<Self>) {
        if self.remote_synced != synced {
            self.remote_synced = synced;
            cx.notify();
        }
    }

    /// A select of refresh choices (plus the stored value if it is not one of them) that stores
    /// the pick for the Volume `identity`, or as the default when there is none.
    fn refresh_select(minutes: u64, identity: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> RefreshSelect {
        let (items, at) = refresh_items(minutes);
        let select = cx.new(|cx| SelectState::new(items, Some(at), window, cx));
        cx.subscribe(&select, move |this, _, event: &SelectEvent<Vec<Every>>, cx| {
            let SelectEvent::Confirm(Some(m)) = event else { return };
            match &identity {
                Some(id) => this.settings.volume_refresh_minutes.insert(id.clone(), *m),
                None => std::mem::replace(&mut this.settings.refresh_minutes, *m).into(),
            };
            this.commit(cx);
        })
        .detach();
        select
    }

    /// A select of day counts (plus the stored one if it is not a choice) that stores the pick as
    /// the Cleaner's age limit for build artefacts, or for temporary files.
    fn days_select(days: u64, build: bool, window: &mut Window, cx: &mut Context<Self>) -> DaysSelect {
        let mut items: Vec<Days> = DAY_CHOICES.map(Days).into();
        if !DAY_CHOICES.contains(&days) {
            items.push(Days(days));
            items.sort_by_key(|d| d.0);
        }
        let at = items.iter().position(|d| d.0 == days).expect("days is among the items");
        let select = cx.new(|cx| SelectState::new(items, Some(IndexPath::new(at)), window, cx));
        cx.subscribe(&select, move |this, _, event: &SelectEvent<Vec<Days>>, cx| {
            let SelectEvent::Confirm(Some(d)) = event else { return };
            *if build {
                &mut this.settings.cleaner_build_days
            } else {
                &mut this.settings.cleaner_temp_days
            } = *d;
            this.commit(cx);
        })
        .detach();
        select
    }

    fn cleaner_limits(&self, cx: &App) -> Div {
        let row = |title: &'static str, note: &'static str, select: &DaysSelect| {
            h_flex()
                .gap(px(16.))
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .child(div().font_weight(FontWeight::MEDIUM).child(title))
                        .child(caption(note, cx)),
                )
                .child(div().w(px(150.)).flex_none().child(Select::new(select).w_full().small()))
        };
        v_flex()
            .gap(px(12.))
            .child(row(
                "Temporary files",
                "Pre-ticked when nothing in them changed for this long",
                &self.temp_days,
            ))
            .child(row(
                "Build artefacts",
                "Pre-ticked when nothing in the project changed for this long",
                &self.build_days,
            ))
    }

    pub fn has_problem(&self) -> bool {
        self.problem.is_some()
    }

    /// Saves the settings and tells the window what to apply.
    fn commit(&mut self, cx: &mut Context<Self>) {
        self.problem = self.settings.save().err().map(|e| format!("Could not save settings: {e}"));
        self.emit_changed(cx);
    }

    fn emit_changed(&mut self, cx: &mut Context<Self>) {
        let indexed: Vec<VolumeInfo> = self.volumes.iter().filter(|v| self.settings.is_indexed(v)).cloned().collect();
        cx.emit(Changed {
            mounted: self.volumes.clone(),
            remotes: self.settings.remotes.clone(),
            intervals: self.settings.intervals(&indexed),
            indexed,
        });
        cx.notify();
    }

    /// Replaces the list of Offline Volumes the Engine reports.
    pub fn set_offline(&mut self, offline: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        if self.offline != offline {
            self.offline = offline;
            cx.notify();
        }
    }

    /// An Offline Volume: its mount, the Offline tag and the Forget link in the reserved slot.
    fn offline_row(&self, ix: usize, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (identity, mount) = self.offline[ix].clone();
        let forget_mount = mount.clone();
        h_flex()
            .min_h(px(62.))
            .py(px(8.))
            .gap(px(16.))
            .items_center()
            .when(!first, |d| d.border_t_1().border_color(p.rule))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().font_weight(FontWeight::MEDIUM).truncate().child(mount.display().to_string()))
                            .child(tag("Offline", Tone::Warning, cx)),
                    )
                    .child(div().text_xs().text_color(p.muted).child("Searchable, not changeable")),
            )
            .child(self.static_field("On plug-in", cx))
            .child(div().w(px(36.)).flex_none())
            .child(
                div()
                    .id(("forget", ix))
                    .w(px(64.))
                    .flex_none()
                    .text_right()
                    .cursor_pointer()
                    .text_color(p.accent)
                    .child("Forget")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.forget(&identity, &forget_mount);
                        this.commit(cx);
                        cx.emit(Forget(identity.clone()));
                    })),
            )
            .into_any_element()
    }

    /// A Remote: its host and path or bucket, the Remote · protocol tag, Last Synced with Refresh now and Edit, and Remove.
    fn remote_row(&self, ix: usize, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let remote = self.settings.remotes[ix].clone();
        let identity = remote.identity();
        let synced = self.remote_synced.get(&identity).copied().flatten();
        let label = last_synced_label(Some(synced), chrono::Utc::now().timestamp()).unwrap_or_default();
        let (refresh_identity, removed, edited) = (identity.clone(), remote.clone(), remote.clone());
        h_flex()
            .min_h(px(62.))
            .py(px(8.))
            .gap(px(16.))
            .items_center()
            .when(!first, |d| d.border_t_1().border_color(p.rule))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().font_weight(FontWeight::MEDIUM).truncate().child(remote.label()))
                            .child(tag(format!("Remote · {}", remote.kind()), Tone::Neutral, cx)),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .text_xs()
                            .text_color(p.muted)
                            .child(format!("{label} · Read-only"))
                            .child(
                                div()
                                    .id(("refresh-remote", ix))
                                    .cursor_pointer()
                                    .text_color(p.accent)
                                    .child("Refresh now")
                                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Refresh(refresh_identity.clone())))),
                            )
                            .child(
                                div()
                                    .id(("edit-remote", ix))
                                    .cursor_pointer()
                                    .text_color(p.accent)
                                    .child("Edit")
                                    .on_click(
                                        cx.listener(move |this, _, window, cx| this.open_remote_dialog(Some(edited.clone()), window, cx)),
                                    ),
                            ),
                    ),
            )
            .child(self.refresh_field(&self.remote_refresh[ix]))
            .child(div().w(px(36.)).flex_none())
            .child(
                div()
                    .id(("remove-remote", ix))
                    .w(px(64.))
                    .flex_none()
                    .text_right()
                    .cursor_pointer()
                    .text_color(p.accent)
                    .child("Remove")
                    .on_click(cx.listener(move |this, _, window, cx| this.remove_remote(&removed, window, cx))),
            )
            .into_any_element()
    }

    fn refresh_field(&self, select: &RefreshSelect) -> Div {
        div().w(px(150.)).flex_none().child(Select::new(select).w_full().small())
    }

    /// The read-only stand-in for a select, as an Offline Volume's refresh interval.
    fn static_field(&self, text: &'static str, cx: &App) -> Div {
        let p = palette(cx);
        h_flex()
            .h(px(32.))
            .w(px(150.))
            .flex_none()
            .px(px(14.))
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(p.control)
            .text_color(p.muted)
            .child(text)
    }

    fn volume_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (rule, muted) = (p.rule, p.muted);
        let volume = &self.volumes[ix];
        let name = crate::app::volume_name(volume);
        let kind = if volume.network {
            "Network share"
        } else if volume.removable {
            "Removable"
        } else {
            "Local Volume"
        };
        let mut notes = vec![volume.mount.display().to_string(), volume.fs_type.clone()];
        if volume.read_only {
            notes.push("read-only".into());
        }
        h_flex()
            .min_h(px(62.))
            .py(px(8.))
            .gap(px(16.))
            .items_center()
            .when(ix > 0, |d| d.border_t_1().border_color(rule))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().font_weight(FontWeight::MEDIUM).child(name))
                            .child(tag(kind, Tone::Neutral, cx)),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(notes.join(" · "))),
            )
            .child(self.refresh_field(&self.volume_refresh[ix]))
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .child(
                        Switch::new(("volume", ix))
                            .checked(self.settings.is_indexed(volume))
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let volume = this.volumes[ix].clone();
                                this.settings.set_indexed(&volume, *checked);
                                this.commit(cx);
                            })),
                    ),
            )
            // Room for the Forget link of an Offline Volume.
            .child(div().w(px(64.)).flex_none())
            .into_any_element()
    }

    fn theme_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        const CHOICES: [(&str, ThemeChoice); 3] = [
            ("System", ThemeChoice::System),
            ("Light", ThemeChoice::Light),
            ("Dark", ThemeChoice::Dark),
        ];
        let p = palette(cx);
        let (ground, border, text, muted) = (p.ground, p.border, p.text, p.muted);
        h_flex()
            .p(px(3.))
            .rounded_full()
            .children(CHOICES.map(|(label, choice)| {
                let on = self.settings.theme == choice;
                div()
                    .id(label)
                    .h(px(32.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .cursor_pointer()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if on { text } else { muted })
                    .when(on, |d| d.bg(ground).border_1().border_color(border))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.theme = choice;
                        theme::apply(choice, cx);
                        this.commit(cx);
                    }))
            }))
            .into_any_element()
    }

    /// The Updates section: the daily-check switch, Check now and what the last check found.
    #[cfg(feature = "updater")]
    fn updates(&self, cx: &mut Context<Self>) -> Div {
        let status = if self.update_status.is_empty() {
            format!("Version {}", crate::update::CURRENT)
        } else {
            self.update_status.clone()
        };
        panel(cx)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .child(
                h_flex()
                    .gap(px(12.))
                    .items_center()
                    .child(
                        Switch::new("update-checks")
                            .checked(self.settings.update_checks)
                            .on_click(cx.listener(|this, on: &bool, _, cx| {
                                this.settings.update_checks = *on;
                                this.commit(cx);
                            })),
                    )
                    .child("Check for updates once a day"),
            )
            .child(
                h_flex()
                    .gap(px(12.))
                    .items_center()
                    .child(div().id("update-status").child(caption(status, cx)))
                    .child(pill("check-now", "Check now").small().build(
                        cx,
                        cx.listener(|this, _, _, cx| {
                            this.update_status = "Checking…".to_owned();
                            cx.emit(CheckNow);
                            cx.notify();
                        }),
                    )),
            )
    }

    /// Shows what an update check found, and remembers when it ran.
    #[cfg(feature = "updater")]
    pub fn update_checked(&mut self, status: String, at: i64, cx: &mut Context<Self>) {
        self.update_status = status;
        self.settings.last_update_check = Some(at);
        // Only the check time changed: nothing for the window to apply.
        self.problem = self.settings.save().err().map(|e| format!("Could not save settings: {e}"));
        cx.notify();
    }

    /// The Remote dialog: SSH, S3, FTP, WebDAV or Azure Blob details for a read-only Remote, to add one or to edit
    /// `editing`. Test tries the details before they are saved.
    pub fn open_remote_dialog(&mut self, editing: Option<Remote>, window: &mut Window, cx: &mut Context<Self>) {
        // An Add after an Edit starts empty; an Add after a cancelled Add keeps what was typed.
        if editing.is_some() || self.editing.is_some() {
            self.fill_dialog(editing.as_ref(), window, cx);
        }
        self.editing = editing;
        self.forget_check(cx);
        let (host, path, every) = (self.remote_host.clone(), self.remote_path.clone(), self.remote_every.clone());
        let (form, ftp_form) = (self.s3.clone(), self.ftp.clone());
        let (dav_form, azure_form) = (self.webdav.clone(), self.azure.clone());
        let page = cx.entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let page = page.clone();
            let (shown_tab, path_style, plain, sas, editing, check) = page.read_with(cx, |p, _| {
                (
                    p.remote_tab,
                    p.s3_path_style,
                    p.ftp_plain,
                    p.azure_sas,
                    p.editing.is_some(),
                    p.check.clone(),
                )
            });
            let p = palette(cx);
            let (surface, ground, border, muted, text) = (p.surface, p.ground, p.border, p.muted, p.text);
            let field = |label: &'static str, input: &Entity<InputState>| {
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(8.))
                    .child(div().font_weight(FontWeight::MEDIUM).child(label))
                    .child(
                        h_flex()
                            .h(px(38.))
                            .px(px(6.))
                            .items_center()
                            .rounded_full()
                            .border_1()
                            .border_color(p.control)
                            .bg(ground)
                            .child(Input::new(input).appearance(false).bordered(false)),
                    )
            };
            let seg = |label: &'static str, on: bool| {
                div()
                    .h(px(32.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .text_color(if on { text } else { muted })
                    .when(on, |d| d.bg(ground).border_1().border_color(border))
                    .child(label)
            };
            let tab = |id: &'static str, label: &'static str, which: RemoteTab| {
                let page = page.clone();
                seg(label, shown_tab == which).id(id).on_click(move |_, window, cx| {
                    page.update(cx, |page, cx| {
                        page.remote_tab = which;
                        page.forget_check(cx);
                    });
                    window.refresh();
                })
            };
            let refresh = Select::new(&every).w_full();
            let refresh = v_flex()
                .gap(px(8.))
                .child(div().font_weight(FontWeight::MEDIUM).child("Refresh"))
                .child(refresh);
            let body =
                if shown_tab == RemoteTab::S3 {
                    v_flex()
                        .gap(px(11.))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Bucket", &form.bucket))
                                .child(field("Prefix", &form.prefix)),
                        )
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Endpoint", &form.endpoint))
                                .child(field("Region", &form.region)),
                        )
                        .child(field("Profile", &form.profile))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Access key", &form.key))
                                .child(field("Secret", &form.secret)),
                        )
                        .child(
                            h_flex()
                                .gap(px(10.))
                                .items_center()
                                .child(Switch::new("s3-path-style").checked(path_style).on_click({
                                    let page = page.clone();
                                    move |checked: &bool, _, cx| {
                                        page.update(cx, |page, cx| {
                                            page.s3_path_style = *checked;
                                            page.forget_check(cx);
                                        })
                                    }
                                }))
                                .child(div().text_color(muted).child("Path-style URLs (MinIO and most custom endpoints)")),
                        )
                        .child(refresh)
                } else if shown_tab == RemoteTab::Ftp {
                    v_flex()
                        .gap(px(11.))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Host", &ftp_form.host))
                                .child(div().w(px(96.)).child(field("Port", &ftp_form.port))),
                        )
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("User", &ftp_form.user))
                                .child(field("Password", &ftp_form.password)),
                        )
                        .child(field("Path", &ftp_form.path))
                        .child(
                            h_flex()
                                .gap(px(10.))
                                .items_center()
                                .child(Switch::new("ftp-allow-plain").checked(plain).on_click({
                                    let page = page.clone();
                                    move |checked: &bool, _, cx| {
                                        page.update(cx, |page, cx| {
                                            page.ftp_plain = *checked;
                                            page.forget_check(cx);
                                        })
                                    }
                                }))
                                .child(div().text_color(muted).child("Allow plain FTP (no encryption)")),
                        )
                        .when(plain, |d| {
                            d.child(div().text_xs().text_color(p.warning).child(
                                "The password and every file name cross the network in clear text. Use only on a network you trust.",
                            ))
                        })
                        .child(refresh)
                } else if shown_tab == RemoteTab::Webdav {
                    v_flex()
                        .gap(px(11.))
                        .child(field("URL", &dav_form.url))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("User", &dav_form.user))
                                .child(field("Password", &dav_form.password)),
                        )
                        .child(refresh)
                } else if shown_tab == RemoteTab::Azure {
                    v_flex()
                        .gap(px(11.))
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Account", &azure_form.account))
                                .child(field("Container", &azure_form.container)),
                        )
                        .child(
                            h_flex()
                                .gap(px(12.))
                                .child(field("Prefix", &azure_form.prefix))
                                .child(field("Endpoint", &azure_form.endpoint)),
                        )
                        .child(field(if sas { "SAS token" } else { "Account key" }, &azure_form.secret))
                        .child(
                            h_flex()
                                .gap(px(10.))
                                .items_center()
                                .child(Switch::new("azure-sas").checked(sas).on_click({
                                    let page = page.clone();
                                    move |checked: &bool, _, cx| {
                                        page.update(cx, |page, cx| {
                                            page.azure_sas = *checked;
                                            page.forget_check(cx);
                                        })
                                    }
                                }))
                                .child(div().text_color(muted).child("The secret is a SAS token, not an account key")),
                        )
                        .child(refresh)
                } else {
                    v_flex()
                        .gap(px(11.))
                        .child(field("Host", &host))
                        .child(field("Path", &path))
                        .child(refresh)
                };
            let outcome = match &check {
                Check::Idle => None,
                Check::Running => Some((muted, "Testing the connection…".to_owned())),
                Check::Passed => Some((p.positive, "Connected: the Remote can be read.".to_owned())),
                Check::Failed(why) => Some((p.negative, why.clone())),
            };
            let save = {
                let (every, page) = (every.clone(), page.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let minutes = every.read(cx).selected_value().copied().unwrap_or(0);
                    let saved = page
                        .read(cx)
                        .typed_remote(cx)
                        .and_then(|(remote, secret)| page.update(cx, |page, cx| page.save_remote(remote, secret, minutes, window, cx)));
                    match saved {
                        Ok(()) => {
                            page.update(cx, |page, cx| page.fill_dialog(None, window, cx));
                            window.close_dialog(cx);
                        }
                        Err(why) => window.push_notification(Notification::error(why), cx),
                    }
                }
            };
            let test = {
                let page = page.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| page.update(cx, |page, cx| page.test_remote(window, cx))
            };
            let tabs = h_flex().child(
                h_flex()
                    .p(px(3.))
                    .rounded_full()
                    .bg(surface)
                    .child(tab("tab-ssh", "SSH", RemoteTab::Ssh))
                    .child(tab("tab-s3", "S3", RemoteTab::S3))
                    .child(tab("tab-ftp", "FTP", RemoteTab::Ftp))
                    .child(tab("tab-webdav", "WebDAV", RemoteTab::Webdav))
                    .child(tab("tab-azure", "Azure", RemoteTab::Azure)),
            );
            dialog
                .w(px(480.))
                .centered()
                .p(px(24.))
                .close_button(false)
                .title(
                    div()
                        .font_family(SERIF_FAMILY)
                        .text_size(px(28.))
                        .line_height(px(32.))
                        .font_weight(FontWeight::NORMAL)
                        .child(if editing { "Edit Remote" } else { "Add Remote" }),
                )
                .child(
                    v_flex()
                        .gap(px(11.))
                        .child(
                            div()
                                .text_color(muted)
                                .child("Remotes are read-only: listed and sized, never changed, no Trash."),
                        )
                        // An edited Remote stays the kind it is.
                        .when(!editing, |d| d.child(tabs))
                        .child(body)
                        .when_some(outcome, |d, (color, line)| d.child(div().text_color(color).child(line))),
                )
                .footer(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().flex_1().text_xs().text_color(muted).child(match shown_tab {
                            RemoteTab::S3 => "Keys left empty: AWS_* or ~/.aws",
                            RemoteTab::Ftp => "Passive mode, FTPS (AUTH TLS) by default",
                            RemoteTab::Webdav => "Basic auth: prefer https",
                            RemoteTab::Azure => "Account key or SAS token",
                            RemoteTab::Ssh => "Keys and ssh-agent only",
                        }))
                        .child(pill("remote-cancel", "Cancel").build(cx, |_, window, cx| window.close_dialog(cx)))
                        .child(
                            pill("remote-test", "Test")
                                .disabled((check == Check::Running).then_some("Testing…"))
                                .build(cx, test),
                        )
                        .child(
                            pill("remote-add", if editing { "Save" } else { "Add Remote" })
                                .primary()
                                .build(cx, save),
                        ),
                )
        });
    }

    /// Puts `remote`'s details in the dialog's fields, tab, switches and refresh interval; `None`
    /// empties them for a new Remote.
    fn fill_dialog(&mut self, remote: Option<&Remote>, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.remote_host, &self.remote_path]
            .into_iter()
            .chain(self.s3.all())
            .chain(self.ftp.all())
            .chain(self.webdav.all())
            .chain(self.azure.all())
        {
            set(input, "", window, cx);
        }
        secret_hint(&self.s3.secret, false, window, cx);
        secret_hint(&self.ftp.password, false, window, cx);
        secret_hint(&self.webdav.password, false, window, cx);
        secret_hint(&self.azure.secret, false, window, cx);
        let minutes = remote.map_or(0, |r| self.settings.remote_refresh_minutes(r));
        self.remote_every.update(cx, |select, cx| {
            let (items, at) = refresh_items(minutes);
            select.set_items(items, window, cx);
            select.set_selected_index(Some(at), window, cx);
        });
        self.s3_path_style = false;
        self.ftp_plain = false;
        self.azure_sas = false;
        match remote {
            None => {}
            Some(Remote::Ssh(r)) => {
                self.remote_tab = RemoteTab::Ssh;
                set(&self.remote_host, &r.host, window, cx);
                set(&self.remote_path, &r.path, window, cx);
            }
            Some(Remote::S3(r)) => {
                self.remote_tab = RemoteTab::S3;
                self.s3_path_style = r.path_style;
                self.s3.fill(r, window, cx);
            }
            Some(Remote::Ftp(r)) => {
                self.remote_tab = RemoteTab::Ftp;
                self.ftp_plain = r.tls == FtpTls::Plain;
                self.ftp.fill(r, window, cx);
            }
            Some(Remote::Webdav(r)) => {
                self.remote_tab = RemoteTab::Webdav;
                self.webdav.fill(r, window, cx);
            }
            Some(Remote::Azure(r)) => {
                self.remote_tab = RemoteTab::Azure;
                self.azure_sas = r.sas;
                self.azure.fill(r, window, cx);
            }
        }
    }
}

fn section(title: &'static str, note: &'static str, body: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .gap(px(16.))
        .items_start()
        .child(
            v_flex()
                .w(px(356.))
                .pr(px(24.))
                .pt(px(3.))
                .flex_none()
                .gap(px(6.))
                .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(title))
                .child(div().text_color(palette(cx).muted).child(note)),
        )
        .child(div().flex_1().min_w_0().child(body))
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (rule, muted, border) = (p.rule, p.muted, p.border);
        let rows: Vec<_> = (0..self.volumes.len()).map(|ix| self.volume_row(ix, cx)).collect();
        let remotes: Vec<_> = (0..self.settings.remotes.len())
            .map(|ix| self.remote_row(ix, rows.is_empty() && ix == 0, cx))
            .collect();
        let offline: Vec<_> = (0..self.offline.len())
            .map(|ix| self.offline_row(ix, rows.is_empty() && remotes.is_empty() && ix == 0, cx))
            .collect();
        let indexed = self.volumes.iter().filter(|v| self.settings.is_indexed(v)).count();
        let add = pill("add-remote", "Add Remote")
            .small()
            .build(cx, cx.listener(|this, _, window, cx| this.open_remote_dialog(None, window, cx)));
        let head = h_flex()
            .pb(px(7.))
            .text_xs()
            .text_color(muted)
            .gap(px(16.))
            .child(div().flex_1().child("Location"))
            .child(div().w(px(150.)).flex_none().child("Refresh interval"))
            .child(div().w(px(36.)).flex_none().child("Index"))
            .child(div().w(px(64.)).flex_none());
        let default = h_flex()
            .mt(px(8.))
            .pt(px(12.))
            .gap(px(16.))
            .items_center()
            .border_t_1()
            .border_color(rule)
            .child(
                v_flex()
                    .flex_1()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Default refresh interval"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("For Volumes without their own interval, re-read while running"),
                    ),
            )
            .child(self.refresh_field(&self.refresh))
            .child(div().w(px(36.)).flex_none())
            .child(div().w(px(64.)).flex_none());
        let locations = panel(cx)
            .child(h_flex().justify_end().mb(px(14.)).child(add))
            .child(head)
            .children(rows)
            .children(remotes)
            .children(offline)
            .child(default);
        let showing = if cx.theme().is_dark() { "dark" } else { "light" };
        let theme = panel(cx)
            .flex()
            .items_center()
            .justify_between()
            .child(self.theme_buttons(cx))
            .child(caption(format!("Showing {showing}"), cx));
        let mut column = v_flex()
            .w_full()
            .max_w(px(1164.))
            .mx_auto()
            .px(px(32.))
            .pt(px(32.))
            .pb(px(48.))
            .gap(px(24.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .mb(px(8.))
                    .child(div().text_size(px(24.)).line_height(px(32.)).child("Settings"))
                    .child(div().text_color(muted).child(format!(
                        "{indexed} of {} Volumes are indexed{}. Changes save as you make them.",
                        self.volumes.len(),
                        match self.settings.remotes.len() {
                            0 => String::new(),
                            1 => ", plus 1 Remote".to_owned(),
                            n => format!(", plus {n} Remotes"),
                        }
                    ))),
            )
            .child(section(
                "Locations",
                "Volumes are indexed by default. Removable Volumes and mounted network shares are opt-in. An Offline Volume stays searchable until you forget it. Remotes are read-only and refresh only when you ask unless you set an interval.",
                locations,
                cx,
            ))
            .child(div().h(px(1.)).bg(border))
            .child(section("Theme", "System follows the operating system and changes with it.", theme, cx))
            .child(div().h(px(1.)).bg(border))
            .child(section(
                "Cleaner",
                "The Cleaner pre-ticks only what has been left alone long enough. Caches and node_modules are never pre-ticked.",
                panel(cx).child(self.cleaner_limits(cx)),
                cx,
            ));
        #[cfg(feature = "updater")]
        {
            let updates = self.updates(cx);
            column = column.child(div().h(px(1.)).bg(border)).child(section(
                "Updates",
                "Asks GitHub Releases for a newer version at most once a day. A failed check only shows here.",
                updates,
                cx,
            ));
        }
        if let Some(problem) = &self.problem {
            column = column.child(div().text_color(p.warning).child(problem.clone()));
        }
        div().id("settings").size_full().overflow_y_scroll().child(column)
    }
}

/// Hot-plug: the window hands the Settings page the Volumes mounted now and changes settings
/// through it, so the page stays the one owner (and writer) of the settings.
impl SettingsPage {
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Takes the Volumes mounted now, with a refresh field for each, and tells the window to
    /// apply the result.
    pub fn set_mounted(&mut self, volumes: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        self.volume_refresh = volumes
            .iter()
            .map(|v| Self::refresh_select(self.settings.refresh_minutes_for(v), Some(v.identity.clone()), window, cx))
            .collect();
        self.volumes = volumes;
        // Nothing to save: only what is mounted changed (and a settings file that could not be
        // read at startup must not be overwritten by a hot-plug).
        self.emit_changed(cx);
    }

    /// Changes the settings with `change`, saves them and tells the window.
    pub fn apply(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        change(&mut self.settings);
        self.commit(cx);
    }
}
