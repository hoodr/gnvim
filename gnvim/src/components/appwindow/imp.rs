use std::cell::RefCell;
use std::rc::Rc;

use futures::lock::Mutex;
use nvim::types::uievents::UiOptions;
use nvim::types::UiEvent;
use once_cell::unsync::OnceCell;

use glib::subclass::InitializingObject;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::CompositeTemplate;
use gtk::{
    gio,
    glib::{self, clone},
};

use gio_compat::CompatRead;
use gio_compat::CompatWrite;
use nvim::rpc::RpcReader;

use crate::colors::{Color, Colors};
use crate::components::shell::Shell;
use crate::font::Font;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/github/vhakulinen/gnvim/application.ui")]
pub struct AppWindow {
    im_context: gtk::IMMulticontext,
    event_controller_key: gtk::EventControllerKey,
    #[template_child(id = "shell")]
    shell: TemplateChild<Shell>,

    nvim: Rc<OnceCell<Mutex<nvim::Client<CompatWrite>>>>,

    colors: Rc<RefCell<Colors>>,
    font: Rc<RefCell<Font>>,
}

impl AppWindow {
    fn open_nvim(&self) -> (nvim::Client<CompatWrite>, CompatRead) {
        let mut flags = gio::SubprocessFlags::empty();
        flags.insert(gio::SubprocessFlags::STDIN_PIPE);
        flags.insert(gio::SubprocessFlags::STDOUT_PIPE);

        let p = gio::Subprocess::newv(
            &[
                std::ffi::OsStr::new("nvim"),
                std::ffi::OsStr::new("--embed"),
            ],
            flags,
        )
        .expect("failed to open nvim subprocess");

        let writer: CompatWrite = p
            .stdin_pipe()
            .expect("get stdin pipe")
            .dynamic_cast::<gio::PollableOutputStream>()
            .expect("cast to PollableOutputStream")
            .into_async_write()
            .expect("convert to async write")
            .into();

        let reader: CompatRead = p
            .stdout_pipe()
            .expect("get stdout pipe")
            .dynamic_cast::<gio::PollableInputStream>()
            .expect("cast to PollableInputStream")
            .into_async_read()
            .expect("covert to async read")
            .into();

        (nvim::Client::new(writer), reader)
    }

    async fn io_loop(&self, reader: CompatRead) {
        use nvim::rpc::{message::Notification, Message};
        let mut reader: RpcReader<CompatRead> = reader.into();

        loop {
            let msg = reader.recv().await.unwrap();
            match msg {
                Message::Response(res) => {
                    self.nvim
                        .get()
                        .expect("nvim client no set")
                        .lock()
                        .await
                        .handle_response(res)
                        .expect("failed to handle nvim response");
                }
                Message::Request(req) => {
                    println!("Got request from nvim: {:?}", req);
                }
                Message::Notification(Notification { method, params, .. }) => {
                    match method.as_ref() {
                        "redraw" => {
                            let events = nvim::decode_redraw_params(params)
                                .expect("failed to decode redraw notification");

                            events
                                .into_iter()
                                .for_each(|event| self.handle_ui_event(event))
                        }
                        _ => {
                            println!("Unexpected notification: {}", method);
                        }
                    }
                }
            }
        }
    }

    fn handle_ui_event(&self, event: UiEvent) {
        match event {
            UiEvent::OptionSet(_) => {}
            UiEvent::DefaultColorsSet(events) => events.into_iter().for_each(|event| {
                let mut colors = self.colors.borrow_mut();
                colors.fg = Color::from_i64(event.rgb_fg);
                colors.bg = Color::from_i64(event.rgb_bg);
                colors.sp = Color::from_i64(event.rgb_sp);
            }),
            UiEvent::HlAttrDefine(events) => events.into_iter().for_each(|event| {
                let mut colors = self.colors.borrow_mut();
                colors.hls.insert(event.id, event.rgb_attrs);
            }),
            UiEvent::HlGroupSet(_) => {}
            UiEvent::GridResize(events) => events.into_iter().for_each(|event| {
                self.shell.handle_grid_resize(event);
            }),
            UiEvent::GridClear(_) => {}
            UiEvent::GridLine(events) => events.into_iter().for_each(|event| {
                self.shell.handle_grid_line(event);
            }),
            UiEvent::UpdateMenu => {}
            UiEvent::WinViewport(_) => {}
            UiEvent::GridCursorGoto(_) => {}
            UiEvent::ModeInfoSet(_) => {}
            UiEvent::ModeChange(_) => {}
            UiEvent::Flush => {
                self.shell
                    .handle_flush(&self.colors.borrow(), &self.font.borrow());
            }
            UiEvent::SetIcon(_) => {}
            UiEvent::SetTitle(_) => {}
            UiEvent::MouseOn => {}
            UiEvent::MouseOff => {}
            event => panic!("Unhandled ui event: {}", event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AppWindow {
    const NAME: &'static str = "AppWindow";
    type Type = super::AppWindow;
    type ParentType = gtk::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        Shell::ensure_type();

        klass.bind_template();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for AppWindow {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        let (client, reader) = self.open_nvim();

        self.nvim
            .set(Mutex::new(client))
            .expect("failed to set nvim");

        let ctx = glib::MainContext::default();
        // Start io loop.
        glib::MainContext::default().spawn_local(clone!(@strong obj as app => async move {
            app.imp().io_loop(reader).await;
        }));

        // Call nvim_ui_attach.
        ctx.spawn_local(clone!(@weak self.nvim as nvim => async move {
            let res = nvim.get()
                    .unwrap()
                    .lock()
                    .await
                    // TODO(ville): Calculate correct size.
                    .nvim_ui_attach(80, 30, UiOptions{
                        rgb: true,
                        ext_linegrid: true,
                        //ext_multigrid: true,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            // TODO(ville): For some reason, if await'ing on the above chain,
            // things just hang. Figure out why.
            res.await.expect("nvim_ui_attach failed");
        }));

        // TODO(ville): Figure out if we should use preedit or not.
        self.im_context.set_use_preedit(false);
        self.event_controller_key
            .set_im_context(Some(&self.im_context));

        self.im_context.connect_commit(|_, input| {
            println!("input: {}", input);
        });

        self.event_controller_key
            .connect_key_pressed(|_, keyval, keycode, state| {
                println!("key pressed: {} {} {}", keyval, keycode, state);
                gtk::Inhibit(false)
            });

        obj.add_controller(&self.event_controller_key);
    }
}

impl WidgetImpl for AppWindow {}

impl WindowImpl for AppWindow {}

impl ApplicationWindowImpl for AppWindow {}