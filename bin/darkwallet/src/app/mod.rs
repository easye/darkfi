/* This file is part of DarkFi (https://dark.fi)
 *
 * Copyright (C) 2020-2024 Dyne.org foundation
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use async_recursion::async_recursion;
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use darkfi_serial::Encodable;
use futures::{stream::FuturesUnordered, StreamExt};
use sled_overlay::sled;
use smol::Task;
use std::{
    sync::{Arc, Mutex as SyncMutex},
    thread,
};

use crate::{
    darkirc::{DarkIrcBackendPtr, Privmsg},
    error::Error,
    expr::Op,
    gfx::{GraphicsEventPublisherPtr, RenderApiPtr, Vertex},
    prop::{Property, PropertyBool, PropertyStr, PropertySubType, PropertyType, Role},
    scene::{
        CallArgType, MethodResponseFn, Pimpl, SceneGraph, SceneGraphPtr2, SceneNodeId,
        SceneNodeType, Slot,
    },
    text::TextShaperPtr,
    ui::{chatview, Button, ChatView, EditBox, Image, Mesh, RenderLayer, Stoppable, Text, Window},
    ExecutorPtr,
};

mod node;
mod schema;

//fn print_type_of<T>(_: &T) {
//    println!("{}", std::any::type_name::<T>())
//}

pub struct AsyncRuntime {
    signal: async_channel::Sender<()>,
    shutdown: async_channel::Receiver<()>,
    exec_threadpool: SyncMutex<Option<thread::JoinHandle<()>>>,
    ex: ExecutorPtr,
    tasks: SyncMutex<Vec<Task<()>>>,
}

impl AsyncRuntime {
    pub fn new(ex: ExecutorPtr) -> Self {
        let (signal, shutdown) = async_channel::unbounded::<()>();

        Self {
            signal,
            shutdown,
            exec_threadpool: SyncMutex::new(None),
            ex,
            tasks: SyncMutex::new(vec![]),
        }
    }

    pub fn start(&self) {
        let n_threads = thread::available_parallelism().unwrap().get();
        let shutdown = self.shutdown.clone();
        let ex = self.ex.clone();
        let exec_threadpool = thread::spawn(move || {
            easy_parallel::Parallel::new()
                // N executor threads
                .each(0..n_threads, |_| smol::future::block_on(ex.run(shutdown.recv())))
                .run();
        });
        *self.exec_threadpool.lock().unwrap() = Some(exec_threadpool);
        debug!(target: "async_runtime", "Started runtime");
    }

    pub fn push_task(&self, task: Task<()>) {
        self.tasks.lock().unwrap().push(task);
    }

    pub fn stop(&self) {
        // Go through event graph and call stop on everything
        // Depth first
        debug!(target: "app", "Stopping async runtime...");

        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        // Close all tasks
        smol::future::block_on(async {
            // Perform cleanup code
            // If not finished in certain amount of time, then just exit

            let futures = FuturesUnordered::new();
            for task in tasks {
                futures.push(task.cancel());
            }
            let _: Vec<_> = futures.collect().await;
        });

        if !self.signal.close() {
            error!(target: "app", "exec threadpool was already shutdown");
        }
        let exec_threadpool = std::mem::replace(&mut *self.exec_threadpool.lock().unwrap(), None);
        let exec_threadpool = exec_threadpool.expect("threadpool wasnt started");
        exec_threadpool.join().unwrap();
        debug!(target: "app", "Stopped app");
    }
}

pub type AppPtr = Arc<App>;

pub struct App {
    pub(self) sg: SceneGraphPtr2,
    pub(self) ex: ExecutorPtr,
    pub(self) render_api: RenderApiPtr,
    pub(self) event_pub: GraphicsEventPublisherPtr,
    pub(self) text_shaper: TextShaperPtr,
    pub(self) darkirc_backend: DarkIrcBackendPtr,
    pub(self) tasks: SyncMutex<Vec<Task<()>>>,
}

impl App {
    pub fn new(
        sg: SceneGraphPtr2,
        ex: ExecutorPtr,
        render_api: RenderApiPtr,
        event_pub: GraphicsEventPublisherPtr,
        text_shaper: TextShaperPtr,
        darkirc_backend: DarkIrcBackendPtr,
    ) -> Arc<Self> {
        Arc::new(Self {
            sg,
            ex,
            render_api,
            event_pub,
            text_shaper,
            darkirc_backend,
            tasks: SyncMutex::new(vec![]),
        })
    }

    pub async fn start(self: Arc<Self>) {
        debug!(target: "app", "App::start()");
        // Setup UI
        let mut sg = self.sg.lock().await;

        let window = sg.add_node("window", SceneNodeType::Window);

        let mut prop = Property::new("screen_size", PropertyType::Float32, PropertySubType::Pixel);
        prop.set_array_len(2);
        // Window not yet initialized so we can't set these.
        //prop.set_f32(Role::App, 0, screen_width);
        //prop.set_f32(Role::App, 1, screen_height);
        window.add_property(prop).unwrap();

        let mut prop = Property::new("scale", PropertyType::Float32, PropertySubType::Pixel);
        prop.set_defaults_f32(vec![1.]).unwrap();
        window.add_property(prop).unwrap();

        let window_id = window.id;

        // Create Window
        // Window::new(window, weak sg)
        drop(sg);
        let pimpl = Window::new(
            self.ex.clone(),
            self.sg.clone(),
            window_id,
            self.render_api.clone(),
            self.event_pub.clone(),
        )
        .await;
        // -> reads any props it needs
        // -> starts procs
        let mut sg = self.sg.lock().await;
        let node = sg.get_node_mut(window_id).unwrap();
        node.pimpl = pimpl;

        sg.link(window_id, SceneGraph::ROOT_ID).unwrap();

        // Testing
        let node = sg.get_node(window_id).unwrap();
        node.set_property_f32(Role::App, "scale", 2.).unwrap();

        drop(sg);

        schema::make(&self).await;
        debug!(target: "app", "Schema loaded");

        // Access drawable in window node and call draw()
        self.trigger_redraw().await;

        // Start the backend
        //if let Err(err) = self.darkirc_backend.start(self.sg.clone(), self.ex.clone()).await {
        //    error!(target: "app", "backend error: {err}");
        //}
    }

    pub fn stop(&self) {
        smol::future::block_on(async {
            self.async_stop().await;
        });
    }

    async fn async_stop(&self) {
        self.darkirc_backend.stop().await;

        let sg = self.sg.lock().await;
        let window_id = sg.lookup_node("/window").unwrap().id;
        self.stop_node(&sg, window_id).await;
        drop(sg);
    }

    #[async_recursion]
    async fn stop_node(&self, sg: &SceneGraph, node_id: SceneNodeId) {
        let node = sg.get_node(node_id).unwrap();
        for child_inf in node.get_children2() {
            self.stop_node(sg, child_inf.id).await;
        }
        match &node.pimpl {
            Pimpl::Window(win) => win.stop().await,
            Pimpl::RenderLayer(layer) => layer.stop().await,
            Pimpl::Mesh(mesh) => mesh.stop().await,
            Pimpl::Text(txt) => txt.stop().await,
            Pimpl::EditBox(ebox) => ebox.stop().await,
            Pimpl::ChatView(_) | Pimpl::Image(_) | Pimpl::Button(_) => {}
            _ => panic!("unhandled pimpl type"),
        };
    }

    async fn trigger_redraw(&self) {
        let sg = self.sg.lock().await;
        let window_node = sg.lookup_node("/window").expect("no window attached!");
        match &window_node.pimpl {
            Pimpl::Window(win) => win.draw(&sg).await,
            _ => panic!("wrong pimpl"),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        debug!(target: "app", "Dropping app");
        // This hangs
        //self.stop();
    }
}

// Just for testing
fn populate_tree(tree: &sled::Tree) {
    let chat_txt = include_str!("../../chat.txt");
    for line in chat_txt.lines() {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        assert_eq!(parts.len(), 3);
        let time_parts: Vec<&str> = parts[0].splitn(2, ':').collect();
        let (hour, min) = (time_parts[0], time_parts[1]);
        let hour = hour.parse::<u32>().unwrap();
        let min = min.parse::<u32>().unwrap();
        let dt: NaiveDateTime =
            NaiveDate::from_ymd_opt(2024, 8, 6).unwrap().and_hms_opt(hour, min, 0).unwrap();
        let timest = dt.and_utc().timestamp_millis() as u64;

        let nick = parts[1].to_string();
        let text = parts[2].to_string();

        // serial order is important here
        let timest = timest.to_be_bytes();
        assert_eq!(timest.len(), 8);
        let mut key = [0u8; 8 + 32];
        key[..8].clone_from_slice(&timest);

        let msg = chatview::ChatMsg { nick, text };
        let mut val = vec![];
        msg.encode(&mut val).unwrap();

        tree.insert(&key, val).unwrap();
    }
    // O(n)
    debug!(target: "app", "populated db with {} lines", tree.len());
}