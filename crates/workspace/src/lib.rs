pub mod pane;
pub mod pane_group;
pub mod settings;
pub mod sidebar;
mod status_bar;

use anyhow::{anyhow, Result};
use client::{Authenticate, ChannelList, Client, User, UserStore};
use gpui::{
    action, elements::*, json::to_string_pretty, keymap::Binding, platform::CursorStyle,
    AnyViewHandle, AppContext, ClipboardItem, Entity, ModelContext, ModelHandle, MutableAppContext,
    PromptLevel, RenderContext, Task, View, ViewContext, ViewHandle, WeakModelHandle,
};
use language::LanguageRegistry;
use log::error;
pub use pane::*;
pub use pane_group::*;
use postage::{prelude::Stream, watch};
use project::{Fs, Project, ProjectPath, Worktree};
pub use settings::Settings;
use sidebar::{Side, Sidebar, SidebarItemId, ToggleSidebarItem, ToggleSidebarItemFocus};
use status_bar::StatusBar;
pub use status_bar::StatusItemView;
use std::{
    collections::{hash_map::Entry, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use theme::Theme;

action!(OpenNew, WorkspaceParams);
action!(Save);
action!(DebugElements);

pub fn init(cx: &mut MutableAppContext) {
    cx.add_action(Workspace::save_active_item);
    cx.add_action(Workspace::debug_elements);
    cx.add_action(Workspace::toggle_sidebar_item);
    cx.add_action(Workspace::toggle_sidebar_item_focus);
    cx.add_bindings(vec![
        Binding::new("cmd-s", Save, None),
        Binding::new("cmd-alt-i", DebugElements, None),
        Binding::new(
            "cmd-shift-!",
            ToggleSidebarItem(SidebarItemId {
                side: Side::Left,
                item_index: 0,
            }),
            None,
        ),
        Binding::new(
            "cmd-1",
            ToggleSidebarItemFocus(SidebarItemId {
                side: Side::Left,
                item_index: 0,
            }),
            None,
        ),
    ]);
    pane::init(cx);
}

pub trait EntryOpener {
    fn open(
        &self,
        worktree: &mut Worktree,
        path: ProjectPath,
        cx: &mut ModelContext<Worktree>,
    ) -> Option<Task<Result<Box<dyn ItemHandle>>>>;
}

pub trait Item: Entity + Sized {
    type View: ItemView;

    fn build_view(
        handle: ModelHandle<Self>,
        settings: watch::Receiver<Settings>,
        cx: &mut ViewContext<Self::View>,
    ) -> Self::View;

    fn project_path(&self) -> Option<ProjectPath>;
}

pub trait ItemView: View {
    fn title(&self, cx: &AppContext) -> String;
    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath>;
    fn clone_on_split(&self, _: &mut ViewContext<Self>) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }
    fn is_dirty(&self, _: &AppContext) -> bool {
        false
    }
    fn has_conflict(&self, _: &AppContext) -> bool {
        false
    }
    fn save(&mut self, cx: &mut ViewContext<Self>) -> Result<Task<Result<()>>>;
    fn save_as(
        &mut self,
        worktree: ModelHandle<Worktree>,
        path: &Path,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>>;
    fn should_activate_item_on_event(_: &Self::Event) -> bool {
        false
    }
    fn should_close_item_on_event(_: &Self::Event) -> bool {
        false
    }
    fn should_update_tab_on_event(_: &Self::Event) -> bool {
        false
    }
}

pub trait ItemHandle: Send + Sync {
    fn add_view(
        &self,
        window_id: usize,
        settings: watch::Receiver<Settings>,
        cx: &mut MutableAppContext,
    ) -> Box<dyn ItemViewHandle>;
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;
    fn downgrade(&self) -> Box<dyn WeakItemHandle>;
    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath>;
}

pub trait WeakItemHandle {
    fn upgrade(&self, cx: &AppContext) -> Option<Box<dyn ItemHandle>>;
}

pub trait ItemViewHandle {
    fn title(&self, cx: &AppContext) -> String;
    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath>;
    fn boxed_clone(&self) -> Box<dyn ItemViewHandle>;
    fn clone_on_split(&self, cx: &mut MutableAppContext) -> Option<Box<dyn ItemViewHandle>>;
    fn set_parent_pane(&self, pane: &ViewHandle<Pane>, cx: &mut MutableAppContext);
    fn id(&self) -> usize;
    fn to_any(&self) -> AnyViewHandle;
    fn is_dirty(&self, cx: &AppContext) -> bool;
    fn has_conflict(&self, cx: &AppContext) -> bool;
    fn save(&self, cx: &mut MutableAppContext) -> Result<Task<Result<()>>>;
    fn save_as(
        &self,
        worktree: ModelHandle<Worktree>,
        path: &Path,
        cx: &mut MutableAppContext,
    ) -> Task<anyhow::Result<()>>;
}

impl<T: Item> ItemHandle for ModelHandle<T> {
    fn add_view(
        &self,
        window_id: usize,
        settings: watch::Receiver<Settings>,
        cx: &mut MutableAppContext,
    ) -> Box<dyn ItemViewHandle> {
        Box::new(cx.add_view(window_id, |cx| T::build_view(self.clone(), settings, cx)))
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn downgrade(&self) -> Box<dyn WeakItemHandle> {
        Box::new(self.downgrade())
    }

    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath> {
        self.read(cx).project_path()
    }
}

impl ItemHandle for Box<dyn ItemHandle> {
    fn add_view(
        &self,
        window_id: usize,
        settings: watch::Receiver<Settings>,
        cx: &mut MutableAppContext,
    ) -> Box<dyn ItemViewHandle> {
        ItemHandle::add_view(self.as_ref(), window_id, settings, cx)
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        self.as_ref().boxed_clone()
    }

    fn downgrade(&self) -> Box<dyn WeakItemHandle> {
        self.as_ref().downgrade()
    }

    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath> {
        self.as_ref().project_path(cx)
    }
}

impl<T: Item> WeakItemHandle for WeakModelHandle<T> {
    fn upgrade(&self, cx: &AppContext) -> Option<Box<dyn ItemHandle>> {
        WeakModelHandle::<T>::upgrade(*self, cx).map(|i| Box::new(i) as Box<dyn ItemHandle>)
    }
}

impl<T: ItemView> ItemViewHandle for ViewHandle<T> {
    fn title(&self, cx: &AppContext) -> String {
        self.read(cx).title(cx)
    }

    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath> {
        self.read(cx).project_path(cx)
    }

    fn boxed_clone(&self) -> Box<dyn ItemViewHandle> {
        Box::new(self.clone())
    }

    fn clone_on_split(&self, cx: &mut MutableAppContext) -> Option<Box<dyn ItemViewHandle>> {
        self.update(cx, |item, cx| {
            cx.add_option_view(|cx| item.clone_on_split(cx))
        })
        .map(|handle| Box::new(handle) as Box<dyn ItemViewHandle>)
    }

    fn set_parent_pane(&self, pane: &ViewHandle<Pane>, cx: &mut MutableAppContext) {
        pane.update(cx, |_, cx| {
            cx.subscribe(self, |pane, item, event, cx| {
                if T::should_close_item_on_event(event) {
                    pane.close_item(item.id(), cx);
                    return;
                }
                if T::should_activate_item_on_event(event) {
                    if let Some(ix) = pane.item_index(&item) {
                        pane.activate_item(ix, cx);
                        pane.activate(cx);
                    }
                }
                if T::should_update_tab_on_event(event) {
                    cx.notify()
                }
            })
            .detach();
        });
    }

    fn save(&self, cx: &mut MutableAppContext) -> Result<Task<Result<()>>> {
        self.update(cx, |item, cx| item.save(cx))
    }

    fn save_as(
        &self,
        worktree: ModelHandle<Worktree>,
        path: &Path,
        cx: &mut MutableAppContext,
    ) -> Task<anyhow::Result<()>> {
        self.update(cx, |item, cx| item.save_as(worktree, path, cx))
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.read(cx).has_conflict(cx)
    }

    fn id(&self) -> usize {
        self.id()
    }

    fn to_any(&self) -> AnyViewHandle {
        self.into()
    }
}

impl Clone for Box<dyn ItemViewHandle> {
    fn clone(&self) -> Box<dyn ItemViewHandle> {
        self.boxed_clone()
    }
}

impl Clone for Box<dyn ItemHandle> {
    fn clone(&self) -> Box<dyn ItemHandle> {
        self.boxed_clone()
    }
}

#[derive(Clone)]
pub struct WorkspaceParams {
    pub client: Arc<Client>,
    pub fs: Arc<dyn Fs>,
    pub languages: Arc<LanguageRegistry>,
    pub settings: watch::Receiver<Settings>,
    pub user_store: ModelHandle<UserStore>,
    pub channel_list: ModelHandle<ChannelList>,
    pub entry_openers: Arc<[Box<dyn EntryOpener>]>,
}

impl WorkspaceParams {
    #[cfg(any(test, feature = "test-support"))]
    pub fn test(cx: &mut MutableAppContext) -> Self {
        let languages = LanguageRegistry::new();
        let client = Client::new();
        let http_client = client::test::FakeHttpClient::new(|_| async move {
            Ok(client::http::ServerResponse::new(404))
        });
        let theme =
            gpui::fonts::with_font_cache(cx.font_cache().clone(), || theme::Theme::default());
        let settings = Settings::new("Courier", cx.font_cache(), Arc::new(theme)).unwrap();
        let user_store = cx.add_model(|cx| UserStore::new(client.clone(), http_client, cx));
        Self {
            channel_list: cx
                .add_model(|cx| ChannelList::new(user_store.clone(), client.clone(), cx)),
            client,
            fs: Arc::new(project::FakeFs::new()),
            languages: Arc::new(languages),
            settings: watch::channel_with(settings).1,
            user_store,
            entry_openers: Arc::from([]),
        }
    }
}

pub struct Workspace {
    pub settings: watch::Receiver<Settings>,
    client: Arc<Client>,
    user_store: ModelHandle<client::UserStore>,
    fs: Arc<dyn Fs>,
    modal: Option<AnyViewHandle>,
    center: PaneGroup,
    left_sidebar: Sidebar,
    right_sidebar: Sidebar,
    panes: Vec<ViewHandle<Pane>>,
    active_pane: ViewHandle<Pane>,
    status_bar: ViewHandle<StatusBar>,
    project: ModelHandle<Project>,
    entry_openers: Arc<[Box<dyn EntryOpener>]>,
    items: Vec<Box<dyn WeakItemHandle>>,
    loading_items: HashMap<
        ProjectPath,
        postage::watch::Receiver<Option<Result<Box<dyn ItemHandle>, Arc<anyhow::Error>>>>,
    >,
    _observe_current_user: Task<()>,
}

impl Workspace {
    pub fn new(params: &WorkspaceParams, cx: &mut ViewContext<Self>) -> Self {
        let project = cx.add_model(|_| {
            Project::new(
                params.languages.clone(),
                params.client.clone(),
                params.user_store.clone(),
                params.fs.clone(),
            )
        });
        cx.observe(&project, |_, _, cx| cx.notify()).detach();

        let pane = cx.add_view(|_| Pane::new(params.settings.clone()));
        let pane_id = pane.id();
        cx.observe(&pane, move |me, _, cx| {
            let active_entry = me.active_project_path(cx);
            me.project
                .update(cx, |project, cx| project.set_active_path(active_entry, cx));
        })
        .detach();
        cx.subscribe(&pane, move |me, _, event, cx| {
            me.handle_pane_event(pane_id, event, cx)
        })
        .detach();
        cx.focus(&pane);

        let status_bar = cx.add_view(|cx| StatusBar::new(&pane, params.settings.clone(), cx));
        let mut current_user = params.user_store.read(cx).watch_current_user().clone();
        let mut connection_status = params.client.status().clone();
        let _observe_current_user = cx.spawn_weak(|this, mut cx| async move {
            current_user.recv().await;
            connection_status.recv().await;
            let mut stream =
                Stream::map(current_user, drop).merge(Stream::map(connection_status, drop));

            while stream.recv().await.is_some() {
                cx.update(|cx| {
                    if let Some(this) = this.upgrade(&cx) {
                        this.update(cx, |_, cx| cx.notify());
                    }
                })
            }
        });

        Workspace {
            modal: None,
            center: PaneGroup::new(pane.id()),
            panes: vec![pane.clone()],
            active_pane: pane.clone(),
            status_bar,
            settings: params.settings.clone(),
            client: params.client.clone(),
            user_store: params.user_store.clone(),
            fs: params.fs.clone(),
            left_sidebar: Sidebar::new(Side::Left),
            right_sidebar: Sidebar::new(Side::Right),
            project,
            entry_openers: params.entry_openers.clone(),
            items: Default::default(),
            loading_items: Default::default(),
            _observe_current_user,
        }
    }

    pub fn left_sidebar_mut(&mut self) -> &mut Sidebar {
        &mut self.left_sidebar
    }

    pub fn right_sidebar_mut(&mut self) -> &mut Sidebar {
        &mut self.right_sidebar
    }

    pub fn status_bar(&self) -> &ViewHandle<StatusBar> {
        &self.status_bar
    }

    pub fn project(&self) -> &ModelHandle<Project> {
        &self.project
    }

    pub fn worktrees<'a>(&self, cx: &'a AppContext) -> &'a [ModelHandle<Worktree>] {
        &self.project.read(cx).worktrees()
    }

    pub fn contains_paths(&self, paths: &[PathBuf], cx: &AppContext) -> bool {
        paths.iter().all(|path| self.contains_path(&path, cx))
    }

    pub fn contains_path(&self, path: &Path, cx: &AppContext) -> bool {
        for worktree in self.worktrees(cx) {
            let worktree = worktree.read(cx).as_local();
            if worktree.map_or(false, |w| w.contains_abs_path(path)) {
                return true;
            }
        }
        false
    }

    pub fn worktree_scans_complete(&self, cx: &AppContext) -> impl Future<Output = ()> + 'static {
        let futures = self
            .worktrees(cx)
            .iter()
            .filter_map(|worktree| worktree.read(cx).as_local())
            .map(|worktree| worktree.scan_complete())
            .collect::<Vec<_>>();
        async move {
            for future in futures {
                future.await;
            }
        }
    }

    pub fn open_paths(&mut self, abs_paths: &[PathBuf], cx: &mut ViewContext<Self>) -> Task<()> {
        let entries = abs_paths
            .iter()
            .cloned()
            .map(|path| self.project_path_for_path(&path, cx))
            .collect::<Vec<_>>();

        let fs = self.fs.clone();
        let tasks = abs_paths
            .iter()
            .cloned()
            .zip(entries.into_iter())
            .map(|(abs_path, project_path)| {
                cx.spawn(|this, mut cx| {
                    let fs = fs.clone();
                    async move {
                        let project_path = project_path.await?;
                        if fs.is_file(&abs_path).await {
                            if let Some(entry) =
                                this.update(&mut cx, |this, cx| this.open_entry(project_path, cx))
                            {
                                entry.await;
                            }
                        }
                        Ok(())
                    }
                })
            })
            .collect::<Vec<Task<Result<()>>>>();

        cx.foreground().spawn(async move {
            for task in tasks {
                if let Err(error) = task.await {
                    log::error!("error opening paths {}", error);
                }
            }
        })
    }

    fn worktree_for_abs_path(
        &self,
        abs_path: &Path,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<(ModelHandle<Worktree>, PathBuf)>> {
        let abs_path: Arc<Path> = Arc::from(abs_path);
        cx.spawn(|this, mut cx| async move {
            let mut entry_id = None;
            this.read_with(&cx, |this, cx| {
                for tree in this.worktrees(cx) {
                    if let Some(relative_path) = tree
                        .read(cx)
                        .as_local()
                        .and_then(|t| abs_path.strip_prefix(t.abs_path()).ok())
                    {
                        entry_id = Some((tree.clone(), relative_path.into()));
                        break;
                    }
                }
            });

            if let Some(entry_id) = entry_id {
                Ok(entry_id)
            } else {
                let worktree = this
                    .update(&mut cx, |this, cx| this.add_worktree(&abs_path, cx))
                    .await?;
                Ok((worktree, PathBuf::new()))
            }
        })
    }

    fn project_path_for_path(
        &self,
        abs_path: &Path,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<ProjectPath>> {
        let entry = self.worktree_for_abs_path(abs_path, cx);
        cx.spawn(|_, _| async move {
            let (worktree, path) = entry.await?;
            Ok(ProjectPath {
                worktree_id: worktree.id(),
                path: path.into(),
            })
        })
    }

    pub fn add_worktree(
        &self,
        path: &Path,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<ModelHandle<Worktree>>> {
        self.project
            .update(cx, |project, cx| project.add_local_worktree(path, cx))
    }

    pub fn toggle_modal<V, F>(&mut self, cx: &mut ViewContext<Self>, add_view: F)
    where
        V: 'static + View,
        F: FnOnce(&mut ViewContext<Self>, &mut Self) -> ViewHandle<V>,
    {
        if self.modal.as_ref().map_or(false, |modal| modal.is::<V>()) {
            self.modal.take();
            cx.focus_self();
        } else {
            let modal = add_view(cx, self);
            cx.focus(&modal);
            self.modal = Some(modal.into());
        }
        cx.notify();
    }

    pub fn modal(&self) -> Option<&AnyViewHandle> {
        self.modal.as_ref()
    }

    pub fn dismiss_modal(&mut self, cx: &mut ViewContext<Self>) {
        if self.modal.take().is_some() {
            cx.focus(&self.active_pane);
            cx.notify();
        }
    }

    #[must_use]
    pub fn open_entry(
        &mut self,
        project_path: ProjectPath,
        cx: &mut ViewContext<Self>,
    ) -> Option<Task<()>> {
        let pane = self.active_pane().clone();
        if self.activate_or_open_existing_entry(project_path.clone(), &pane, cx) {
            return None;
        }

        let worktree = match self
            .project
            .read(cx)
            .worktree_for_id(project_path.worktree_id)
        {
            Some(worktree) => worktree,
            None => {
                log::error!("worktree {} does not exist", project_path.worktree_id);
                return None;
            }
        };

        if let Entry::Vacant(entry) = self.loading_items.entry(project_path.clone()) {
            let (mut tx, rx) = postage::watch::channel();
            entry.insert(rx);

            let project_path = project_path.clone();
            let entry_openers = self.entry_openers.clone();
            cx.as_mut()
                .spawn(|mut cx| async move {
                    let item = worktree.update(&mut cx, move |worktree, cx| {
                        for opener in entry_openers.iter() {
                            if let Some(task) = opener.open(worktree, project_path.clone(), cx) {
                                return task;
                            }
                        }

                        cx.spawn(|_, _| async move {
                            Err(anyhow!("no opener for path {:?} found", project_path))
                        })
                    });
                    *tx.borrow_mut() = Some(item.await.map_err(Arc::new));
                })
                .detach();
        }

        let pane = pane.downgrade();
        let mut watch = self.loading_items.get(&project_path).unwrap().clone();

        Some(cx.spawn(|this, mut cx| async move {
            let load_result = loop {
                if let Some(load_result) = watch.borrow().as_ref() {
                    break load_result.clone();
                }
                watch.recv().await;
            };

            this.update(&mut cx, |this, cx| {
                this.loading_items.remove(&project_path);
                if let Some(pane) = pane.upgrade(&cx) {
                    match load_result {
                        Ok(item) => {
                            // By the time loading finishes, the entry could have been already added
                            // to the pane. If it was, we activate it, otherwise we'll store the
                            // item and add a new view for it.
                            if !this.activate_or_open_existing_entry(project_path, &pane, cx) {
                                this.add_item(item, cx);
                            }
                        }
                        Err(error) => {
                            log::error!("error opening item: {}", error);
                        }
                    }
                }
            })
        }))
    }

    fn activate_or_open_existing_entry(
        &mut self,
        project_path: ProjectPath,
        pane: &ViewHandle<Pane>,
        cx: &mut ViewContext<Self>,
    ) -> bool {
        // If the pane contains a view for this file, then activate
        // that item view.
        if pane.update(cx, |pane, cx| pane.activate_entry(project_path.clone(), cx)) {
            return true;
        }

        // Otherwise, if this file is already open somewhere in the workspace,
        // then add another view for it.
        let settings = self.settings.clone();
        let mut view_for_existing_item = None;
        self.items.retain(|item| {
            if let Some(item) = item.upgrade(cx) {
                if view_for_existing_item.is_none()
                    && item
                        .project_path(cx)
                        .map_or(false, |item_project_path| item_project_path == project_path)
                {
                    view_for_existing_item =
                        Some(item.add_view(cx.window_id(), settings.clone(), cx.as_mut()));
                }
                true
            } else {
                false
            }
        });
        if let Some(view) = view_for_existing_item {
            pane.add_item_view(view, cx.as_mut());
            true
        } else {
            false
        }
    }

    pub fn active_item(&self, cx: &AppContext) -> Option<Box<dyn ItemViewHandle>> {
        self.active_pane().read(cx).active_item()
    }

    fn active_project_path(&self, cx: &ViewContext<Self>) -> Option<ProjectPath> {
        self.active_item(cx).and_then(|item| item.project_path(cx))
    }

    pub fn save_active_item(&mut self, _: &Save, cx: &mut ViewContext<Self>) {
        if let Some(item) = self.active_item(cx) {
            let handle = cx.handle();
            if item.project_path(cx.as_ref()).is_none() {
                let worktree = self.worktrees(cx).first();
                let start_abs_path = worktree
                    .and_then(|w| w.read(cx).as_local())
                    .map_or(Path::new(""), |w| w.abs_path())
                    .to_path_buf();
                cx.prompt_for_new_path(&start_abs_path, move |abs_path, cx| {
                    if let Some(abs_path) = abs_path {
                        cx.spawn(|mut cx| async move {
                            let result = match handle
                                .update(&mut cx, |this, cx| {
                                    this.worktree_for_abs_path(&abs_path, cx)
                                })
                                .await
                            {
                                Ok((worktree, path)) => {
                                    handle
                                        .update(&mut cx, |_, cx| {
                                            item.save_as(worktree, &path, cx.as_mut())
                                        })
                                        .await
                                }
                                Err(error) => Err(error),
                            };

                            if let Err(error) = result {
                                error!("failed to save item: {:?}, ", error);
                            }
                        })
                        .detach()
                    }
                });
                return;
            } else if item.has_conflict(cx.as_ref()) {
                const CONFLICT_MESSAGE: &'static str = "This file has changed on disk since you started editing it. Do you want to overwrite it?";

                cx.prompt(
                    PromptLevel::Warning,
                    CONFLICT_MESSAGE,
                    &["Overwrite", "Cancel"],
                    move |answer, cx| {
                        if answer == 0 {
                            cx.spawn(|mut cx| async move {
                                if let Err(error) = cx.update(|cx| item.save(cx)).unwrap().await {
                                    error!("failed to save item: {:?}, ", error);
                                }
                            })
                            .detach();
                        }
                    },
                );
            } else {
                cx.spawn(|_, mut cx| async move {
                    if let Err(error) = cx.update(|cx| item.save(cx)).unwrap().await {
                        error!("failed to save item: {:?}, ", error);
                    }
                })
                .detach();
            }
        }
    }

    pub fn toggle_sidebar_item(&mut self, action: &ToggleSidebarItem, cx: &mut ViewContext<Self>) {
        let sidebar = match action.0.side {
            Side::Left => &mut self.left_sidebar,
            Side::Right => &mut self.right_sidebar,
        };
        sidebar.toggle_item(action.0.item_index);
        if let Some(active_item) = sidebar.active_item() {
            cx.focus(active_item);
        } else {
            cx.focus_self();
        }
        cx.notify();
    }

    pub fn toggle_sidebar_item_focus(
        &mut self,
        action: &ToggleSidebarItemFocus,
        cx: &mut ViewContext<Self>,
    ) {
        let sidebar = match action.0.side {
            Side::Left => &mut self.left_sidebar,
            Side::Right => &mut self.right_sidebar,
        };
        sidebar.activate_item(action.0.item_index);
        if let Some(active_item) = sidebar.active_item() {
            if active_item.is_focused(cx) {
                cx.focus_self();
            } else {
                cx.focus(active_item);
            }
        }
        cx.notify();
    }

    pub fn debug_elements(&mut self, _: &DebugElements, cx: &mut ViewContext<Self>) {
        match to_string_pretty(&cx.debug_elements()) {
            Ok(json) => {
                let kib = json.len() as f32 / 1024.;
                cx.as_mut().write_to_clipboard(ClipboardItem::new(json));
                log::info!(
                    "copied {:.1} KiB of element debug JSON to the clipboard",
                    kib
                );
            }
            Err(error) => {
                log::error!("error debugging elements: {}", error);
            }
        };
    }

    fn add_pane(&mut self, cx: &mut ViewContext<Self>) -> ViewHandle<Pane> {
        let pane = cx.add_view(|_| Pane::new(self.settings.clone()));
        let pane_id = pane.id();
        cx.observe(&pane, move |me, _, cx| {
            let active_entry = me.active_project_path(cx);
            me.project
                .update(cx, |project, cx| project.set_active_path(active_entry, cx));
        })
        .detach();
        cx.subscribe(&pane, move |me, _, event, cx| {
            me.handle_pane_event(pane_id, event, cx)
        })
        .detach();
        self.panes.push(pane.clone());
        self.activate_pane(pane.clone(), cx);
        pane
    }

    pub fn add_item<T>(&mut self, item_handle: T, cx: &mut ViewContext<Self>)
    where
        T: ItemHandle,
    {
        let view = item_handle.add_view(cx.window_id(), self.settings.clone(), cx);
        self.items.push(item_handle.downgrade());
        self.active_pane().add_item_view(view, cx.as_mut());
    }

    fn activate_pane(&mut self, pane: ViewHandle<Pane>, cx: &mut ViewContext<Self>) {
        self.active_pane = pane;
        self.status_bar.update(cx, |status_bar, cx| {
            status_bar.set_active_pane(&self.active_pane, cx);
        });
        cx.focus(&self.active_pane);
        cx.notify();
    }

    fn handle_pane_event(
        &mut self,
        pane_id: usize,
        event: &pane::Event,
        cx: &mut ViewContext<Self>,
    ) {
        if let Some(pane) = self.pane(pane_id) {
            match event {
                pane::Event::Split(direction) => {
                    self.split_pane(pane, *direction, cx);
                }
                pane::Event::Remove => {
                    self.remove_pane(pane, cx);
                }
                pane::Event::Activate => {
                    self.activate_pane(pane, cx);
                }
            }
        } else {
            error!("pane {} not found", pane_id);
        }
    }

    pub fn split_pane(
        &mut self,
        pane: ViewHandle<Pane>,
        direction: SplitDirection,
        cx: &mut ViewContext<Self>,
    ) -> ViewHandle<Pane> {
        let new_pane = self.add_pane(cx);
        self.activate_pane(new_pane.clone(), cx);
        if let Some(item) = pane.read(cx).active_item() {
            if let Some(clone) = item.clone_on_split(cx.as_mut()) {
                new_pane.add_item_view(clone, cx.as_mut());
            }
        }
        self.center
            .split(pane.id(), new_pane.id(), direction)
            .unwrap();
        cx.notify();
        new_pane
    }

    fn remove_pane(&mut self, pane: ViewHandle<Pane>, cx: &mut ViewContext<Self>) {
        if self.center.remove(pane.id()).unwrap() {
            self.panes.retain(|p| p != &pane);
            self.activate_pane(self.panes.last().unwrap().clone(), cx);
        }
    }

    pub fn panes(&self) -> &[ViewHandle<Pane>] {
        &self.panes
    }

    fn pane(&self, pane_id: usize) -> Option<ViewHandle<Pane>> {
        self.panes.iter().find(|pane| pane.id() == pane_id).cloned()
    }

    pub fn active_pane(&self) -> &ViewHandle<Pane> {
        &self.active_pane
    }

    fn render_connection_status(&self) -> Option<ElementBox> {
        let theme = &self.settings.borrow().theme;
        match &*self.client.status().borrow() {
            client::Status::ConnectionError
            | client::Status::ConnectionLost
            | client::Status::Reauthenticating
            | client::Status::Reconnecting { .. }
            | client::Status::ReconnectionError { .. } => Some(
                Container::new(
                    Align::new(
                        ConstrainedBox::new(
                            Svg::new("icons/offline-14.svg")
                                .with_color(theme.workspace.titlebar.icon_color)
                                .boxed(),
                        )
                        .with_width(theme.workspace.titlebar.offline_icon.width)
                        .boxed(),
                    )
                    .boxed(),
                )
                .with_style(theme.workspace.titlebar.offline_icon.container)
                .boxed(),
            ),
            client::Status::UpgradeRequired => Some(
                Label::new(
                    "Please update Zed to collaborate".to_string(),
                    theme.workspace.titlebar.outdated_warning.text.clone(),
                )
                .contained()
                .with_style(theme.workspace.titlebar.outdated_warning.container)
                .aligned()
                .boxed(),
            ),
            _ => None,
        }
    }

    fn render_titlebar(&self, theme: &Theme, cx: &mut RenderContext<Self>) -> ElementBox {
        ConstrainedBox::new(
            Container::new(
                Stack::new()
                    .with_child(
                        Align::new(
                            Label::new("zed".into(), theme.workspace.titlebar.title.clone())
                                .boxed(),
                        )
                        .boxed(),
                    )
                    .with_child(
                        Align::new(
                            Flex::row()
                                .with_children(self.render_collaborators(theme, cx))
                                .with_child(
                                    self.render_avatar(
                                        self.user_store.read(cx).current_user().as_ref(),
                                        self.project
                                            .read(cx)
                                            .active_worktree()
                                            .map(|worktree| worktree.read(cx).replica_id()),
                                        theme,
                                        cx,
                                    ),
                                )
                                .with_children(self.render_connection_status())
                                .boxed(),
                        )
                        .right()
                        .boxed(),
                    )
                    .boxed(),
            )
            .with_style(theme.workspace.titlebar.container)
            .boxed(),
        )
        .with_height(theme.workspace.titlebar.height)
        .named("titlebar")
    }

    fn render_collaborators(&self, theme: &Theme, cx: &mut RenderContext<Self>) -> Vec<ElementBox> {
        let mut elements = Vec::new();
        if let Some(active_worktree) = self.project.read(cx).active_worktree() {
            let collaborators = active_worktree
                .read(cx)
                .collaborators()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for collaborator in collaborators {
                elements.push(self.render_avatar(
                    Some(&collaborator.user),
                    Some(collaborator.replica_id),
                    theme,
                    cx,
                ));
            }
        }
        elements
    }

    fn render_avatar(
        &self,
        user: Option<&Arc<User>>,
        replica_id: Option<u16>,
        theme: &Theme,
        cx: &mut RenderContext<Self>,
    ) -> ElementBox {
        if let Some(avatar) = user.and_then(|user| user.avatar.clone()) {
            ConstrainedBox::new(
                Stack::new()
                    .with_child(
                        ConstrainedBox::new(
                            Image::new(avatar)
                                .with_style(theme.workspace.titlebar.avatar)
                                .boxed(),
                        )
                        .with_width(theme.workspace.titlebar.avatar_width)
                        .aligned()
                        .boxed(),
                    )
                    .with_child(
                        Container::new(Empty::new().boxed())
                            .with_style(theme.workspace.titlebar.avatar_ribbon.container)
                            .with_background_color(replica_id.map_or(Default::default(), |id| {
                                theme.editor.replica_selection_style(id).cursor
                            }))
                            .constrained()
                            .with_width(theme.workspace.titlebar.avatar_ribbon.width)
                            .with_height(theme.workspace.titlebar.avatar_ribbon.height)
                            .aligned()
                            .bottom()
                            .boxed(),
                    )
                    .boxed(),
            )
            .with_width(theme.workspace.right_sidebar.width)
            .boxed()
        } else {
            MouseEventHandler::new::<Authenticate, _, _, _>(0, cx, |state, _| {
                let style = if state.hovered {
                    &theme.workspace.titlebar.hovered_sign_in_prompt
                } else {
                    &theme.workspace.titlebar.sign_in_prompt
                };
                Label::new("Sign in".to_string(), style.text.clone())
                    .contained()
                    .with_style(style.container)
                    .boxed()
            })
            .on_click(|cx| cx.dispatch_action(Authenticate))
            .with_cursor_style(CursorStyle::PointingHand)
            .aligned()
            .boxed()
        }
    }
}

impl Entity for Workspace {
    type Event = ();
}

impl View for Workspace {
    fn ui_name() -> &'static str {
        "Workspace"
    }

    fn render(&mut self, cx: &mut RenderContext<Self>) -> ElementBox {
        let settings = self.settings.borrow();
        let theme = &settings.theme;
        Container::new(
            Flex::column()
                .with_child(self.render_titlebar(&theme, cx))
                .with_child(
                    Expanded::new(
                        1.0,
                        Stack::new()
                            .with_child({
                                let mut content = Flex::row();
                                content.add_child(self.left_sidebar.render(&settings, cx));
                                if let Some(element) =
                                    self.left_sidebar.render_active_item(&settings, cx)
                                {
                                    content.add_child(Flexible::new(0.8, element).boxed());
                                }
                                content.add_child(
                                    Flex::column()
                                        .with_child(
                                            Expanded::new(1.0, self.center.render(&settings.theme))
                                                .boxed(),
                                        )
                                        .with_child(ChildView::new(self.status_bar.id()).boxed())
                                        .expanded(1.)
                                        .boxed(),
                                );
                                if let Some(element) =
                                    self.right_sidebar.render_active_item(&settings, cx)
                                {
                                    content.add_child(Flexible::new(0.8, element).boxed());
                                }
                                content.add_child(self.right_sidebar.render(&settings, cx));
                                content.boxed()
                            })
                            .with_children(
                                self.modal.as_ref().map(|m| ChildView::new(m.id()).boxed()),
                            )
                            .boxed(),
                    )
                    .boxed(),
                )
                .boxed(),
        )
        .with_background_color(settings.theme.workspace.background)
        .named("workspace")
    }

    fn on_focus(&mut self, cx: &mut ViewContext<Self>) {
        cx.focus(&self.active_pane);
    }
}

pub trait WorkspaceHandle {
    fn file_project_paths(&self, cx: &AppContext) -> Vec<ProjectPath>;
}

impl WorkspaceHandle for ViewHandle<Workspace> {
    fn file_project_paths(&self, cx: &AppContext) -> Vec<ProjectPath> {
        self.read(cx)
            .worktrees(cx)
            .iter()
            .flat_map(|worktree| {
                let worktree_id = worktree.id();
                worktree.read(cx).files(true, 0).map(move |f| ProjectPath {
                    worktree_id,
                    path: f.path.clone(),
                })
            })
            .collect::<Vec<_>>()
    }
}
