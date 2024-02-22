use futures::{SinkExt, StreamExt, TryFutureExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, RwLock};
use tokio::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::ws::{Message, WebSocket};
use warp::{http::Uri, Filter, Rejection, Reply};

static NEXT_POLL_CONNECTION_ID: AtomicUsize = AtomicUsize::new(1);
static NEXT_SCREEN_CONNECTION_ID: AtomicUsize = AtomicUsize::new(1);

type Connections = HashMap<usize, mpsc::UnboundedSender<Message>>;

#[derive(Debug)]
struct Poll {
    name: String,         // 投票的显示名称，用于前端渲染
    options: Vec<String>, // 选项名称在数组中的索引，最多支持256个选项
    polls: Vec<u16>,      // 各选项的票数
}

impl Poll {
    fn new(name: String, options_in: Vec<String>) -> Self {
        Self {
            name,
            options: options_in.clone(),
            polls: vec![0; options_in.len()],
        }
    }

    // fn reset(&mut self) {
    //     self.polls = vec![0; self.options.len()];
    // }

    fn poll(&mut self, option: usize) {
        if option < self.polls.len() {
            self.polls[option] += 1;
        }
    }
}

#[derive(Default)]
struct AppState {
    polls: HashMap<String, Poll>,
    current: Option<String>,
    poll_connections: Connections,
    screen_connections: Connections,
}

type AppStateArc = Arc<RwLock<AppState>>;

#[tokio::main]
async fn main() {
    const SCREEN_HTML: &'static str = include_str!("static/screen.html");
    const POLL_HTML: &'static str = include_str!("static/poll.html");
    const CONTROL_HTML: &'static str = include_str!("static/control.html");
    const ECHARTS_JS: &'static str = include_str!("static/echarts.min.js");
    const PICO_CSS: &'static str = include_str!("static/pico.red.min.css");
    const CONTETTI_JS: &'static str = include_str!("static/tsparticles.confetti.bundle.min.js");

    let app_state = AppStateArc::default();
    let app_state_clone = app_state.clone();
    let with_state = warp::any().map(move || app_state.clone());

    // WebSocket routes for polls and screens
    let screen_ws_route = warp::path!("websocket" / "screen")
        .and(warp::ws())
        // .and(screen_connections_filter)
        .and(with_state.clone())
        .map(|ws: warp::ws::Ws, with_state| {
            // And then our closure will be called when it completes...
            ws.on_upgrade(move |socket| screen_connected(socket, with_state))
        });

    let poll_ws_route = warp::path!("websocket" / "poll")
        .and(warp::ws())
        // .and(screen_connections_filter)
        .and(with_state.clone())
        .map(|ws: warp::ws::Ws, with_state| {
            // And then our closure will be called when it completes...
            ws.on_upgrade(move |socket| poll_connected(socket, with_state))
        });

    //

    // WebSocket 路由
    // let screen_ws_route = warp::path!("websocket" / "screen")
    //     .and(warp::ws())
    //     .map(move |ws: warp::ws::Ws| {
    //         let tx = screen_tx.subscribe();

    //         ws.on_upgrade(move |websocket| {
    //             let (mut screen_tx, rx) = websocket.split();

    //             // Spawn a task to forward messages from the screen broadcast receiver to this WebSocket
    //             tokio::spawn(async move {
    //                 while let Ok(message) = tx.recv().await {
    //                     if screen_tx.send(message).is_err() {
    //                         break;
    //                     }
    //                 }
    //             });

    //             // Process incoming messages
    //             let process_messages = rx.for_each(move |message| {
    //                 // Process screen messages
    //                 println!("Screen Received: {}", message);
    //                 Ok(())
    //             });

    //             // Combine the two futures
    //             let combined = process_messages.map(|_| ());

    //             // Return a future that completes when the WebSocket connection is closed
    //             combined
    //         })
    //     });

    // let ws_poll_route = warp::path("ws/poll")
    //     .and(warp::ws())
    //     .and(warp::any().map(move || app_state.clone()))
    //     .map(handle_websocket);

    // let ws_screen_route = warp::path("ws/screen")
    //     .and(warp::ws())
    //     .and(warp::any().map(move || app_state.clone()))
    //     .map({handle_websocket});

    // POST 路由
    let create_poll_route = warp::path!("api" / "create_poll")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state.clone())
        .and_then(handle_create_poll);

    let start_poll_route = warp::path!("api" / "start_poll")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state.clone())
        .and_then(handle_start_poll);

    let stop_poll_route = warp::path!("api" / "stop_poll")
        .and(warp::post())
        .and(with_state.clone())
        .and_then(handle_stop_poll);

    // GET 路由
    let poll_route = warp::path("poll").map(move || warp::reply::html(POLL_HTML));
    let screen_route = warp::path("screen").map(move || warp::reply::html(SCREEN_HTML));
    let control_route = warp::path("control").map(move || warp::reply::html(CONTROL_HTML));

    // 资源路由
    let echarts_route = warp::path!("resource" / "echarts.min.js").map(move || {
        warp::reply::with_header(ECHARTS_JS, "content-type", "text/javascript; charset=utf-8")
    });

    let picocss_route = warp::path!("resource" / "pico.red.min.css")
        .map(move || warp::reply::with_header(PICO_CSS, "content-type", "text/css; charset=utf-8"));

    let confetti_route =
        warp::path!("resource" / "tsparticles.confetti.bundle.min.js").map(move || {
            warp::reply::with_header(
                CONTETTI_JS,
                "content-type",
                "text/javascript; charset=utf-8",
            )
        });

    // 默认路由
    let default_route = warp::path::end().map(|| warp::redirect(Uri::from_static("/poll")));

    // 合并路由
    let routes = default_route
        .or(create_poll_route)
        .or(start_poll_route)
        .or(stop_poll_route)
        //
        .or(poll_ws_route)
        .or(screen_ws_route)
        //
        .or(poll_route)
        .or(screen_route)
        .or(control_route)
        //
        .or(echarts_route)
        .or(picocss_route)
        .or(confetti_route);

        println!("我发誓之后一定会把Print补上...");
    tokio::spawn(handle_connection(app_state_clone));
    warp::serve(routes).run(([0, 0, 0, 0], 3030)).await;
}

#[derive(Debug, Deserialize)]
struct CreatePollData {
    id: String,
    name: String,
    options: Vec<String>,
}

async fn handle_create_poll(
    json: CreatePollData,
    app_state_clone: AppStateArc,
) -> Result<impl Reply, Rejection> {
    // 加一个存在判定
    app_state_clone
        .write()
        .await
        .polls
        .insert(json.id, Poll::new(json.name, json.options));
    Ok(warp::reply::html("请求成功"))
}

#[derive(Debug, Deserialize)]
struct StartPollData {
    id: String,
}

async fn handle_start_poll(
    json: StartPollData,
    app_state_clone: AppStateArc,
) -> Result<impl Reply, Rejection> {
    if app_state_clone.read().await.polls.contains_key(&json.id) {
        app_state_clone.write().await.current = Some(json.id.to_string());

        let new_json = json!({
            "status": 1,
            "title": app_state_clone.read().await.polls.get(&json.id).unwrap().name,
            "options": app_state_clone.read().await.polls.get(&json.id).unwrap().options,
        });
        screen_message(new_json.to_string().as_str(), &app_state_clone).await;
        poll_message(new_json.to_string().as_str(), &app_state_clone).await;

        // handle_connection(app_state_clone, &screen_connections);
        return Ok(warp::reply::html(new_json.to_string()));
    }
    Ok(warp::reply::html("未找到指定的Poll".to_string()))
}

async fn handle_stop_poll(app_state_clone: AppStateArc) -> Result<impl Reply, Rejection> {
    match &app_state_clone.read().await.current {
        Some(_current) => {
            let new_json = json!({
                "status": 2
            });
            screen_message(new_json.to_string().as_str(), &app_state_clone).await;
            poll_message(new_json.to_string().as_str(), &app_state_clone).await;
        }
        None => return Ok(warp::reply::html("投票本身并未开始".to_string())),
    }
    app_state_clone.write().await.current = None;
    Ok(warp::reply::html("投票已结束".to_string()))
}

// async fn start_poll(data: PollData) -> Result<impl Reply, Rejection> {
//     // Implement your logic for starting a poll
//     Ok(warp::reply::json(&data))
// }

// async fn stop_poll(data: PollData) -> Result<impl Reply, Rejection> {
//     // Implement your logic for stopping a poll
//     Ok(warp::reply::json(&data))
// }

// async fn list_poll(data: PollData) -> Result<impl Reply, Rejection> {
//     // Implement your logic for listing polls
//     Ok(warp::reply::json(&data))
// }

// async fn delete_poll(data: PollData) -> Result<impl Reply, Rejection> {
//     // Implement your logic for deleting a poll
//     Ok(warp::reply::json(&data))
// }

// async fn export_poll(data: PollData) -> Result<impl Reply, Rejection> {
//     // Implement your logic for exporting a poll
//     Ok(warp::reply::json(&data))
// }

async fn screen_connected(ws: WebSocket, app_state_clone: AppStateArc) {
    // let screen_connections = &app_state_clone.lock().await.screen_connections;
    let id = NEXT_SCREEN_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    println!("New Screen Connection: {}", id);
    let (mut screen_ws_tx, mut screen_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut rx = UnboundedReceiverStream::new(rx);

    tokio::task::spawn(async move {
        while let Some(message) = rx.next().await {
            screen_ws_tx
                .send(message)
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
                .await;
        }
    });

    match &app_state_clone.read().await.current {
        Some(current) => {
            let new_json = json!({
                "status": 1,
                "title": app_state_clone.read().await.polls.get(current).unwrap().name,
                "options": app_state_clone.read().await.polls.get(current).unwrap().options,
            });
            let _ = tx.send(Message::text(new_json.to_string().as_str()));
        }
        None => {
            println!("No value found");
        }
    }
    app_state_clone
        .write()
        .await
        .screen_connections
        .insert(id, tx);
    while let Some(result) = screen_ws_rx.next().await {
        let _msg = match result {
            Ok(msg) => {
                if msg.is_close() {
                    break;
                }
            }
            Err(e) => {
                eprintln!("websocket Error(id={}): {}", id, e);
                break;
            }
        };
    }

    screen_disconnected(id, &app_state_clone).await;
}

async fn screen_message(msg: &str, app_state_clone: &AppStateArc) {
    for (&_uid, tx) in app_state_clone.read().await.screen_connections.iter() {
        if let Err(_disconnected) = tx.send(Message::text(msg)) {}
    }
}

async fn screen_disconnected(id: usize, app_state_clone: &AppStateArc) {
    println!("Screen Disconnected: {}", id);
    app_state_clone.write().await.screen_connections.remove(&id);
}

async fn handle_connection(app_state_clone: AppStateArc) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        match &app_state_clone.read().await.current {
            Some(current) => {
                // let new_json = json!({
                //     "title": app_state_clone.read().await.polls.get(current).unwrap().name,
                //     "options": app_state_clone.read().await.polls.get(current).unwrap().options,
                // });
                let new_json = json!({
                    "status": 0,
                    "data": app_state_clone.read().await.polls.get(current).unwrap().polls,
                });

                screen_message(new_json.to_string().as_str(), &app_state_clone).await;
            }
            None => {}
        }
    }
}

async fn poll_connected(ws: WebSocket, app_state_clone: AppStateArc) {
    // let screen_connections = &app_state_clone.lock().await.screen_connections;
    let id = NEXT_POLL_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    println!("New POLL Connection: {}", id);
    let (mut poll_ws_tx, mut poll_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut rx = UnboundedReceiverStream::new(rx);

    tokio::task::spawn(async move {
        while let Some(message) = rx.next().await {
            poll_ws_tx
                .send(message)
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
                .await;
        }
    });

    match &app_state_clone.read().await.current {
        Some(current) => {
            let new_json = json!({
                "status": 1,
                "title": app_state_clone.read().await.polls.get(current).unwrap().name,
                "options": app_state_clone.read().await.polls.get(current).unwrap().options,
            });
            let _ = tx.send(Message::text(new_json.to_string().as_str()));
        }
        None => {
            let new_json = json!({
                "status": 2
            });
            let _ = tx.send(Message::text(new_json.to_string().as_str()));
        }
    }
    app_state_clone
        .write()
        .await
        .poll_connections
        .insert(id, tx);

    while let Some(result) = poll_ws_rx.next().await {
        let _msg = match result {
            Ok(msg) => {
                match serde_json::from_str::<Value>(match msg.to_str() {
                    Ok(value) => value,
                    Err(()) => "",
                }) {
                    Ok(parsed_json) => {
                        // 提取 poll 字段的整数值
                        if let Some(poll_value) = parsed_json.get("poll") {
                            if let Some(poll_int) = poll_value.as_u64() {
                                if app_state_clone.read().await.current.is_some() {
                                    let a = app_state_clone.read().await.current.clone().unwrap();
                                    let mut app_state = app_state_clone.write().await;
                                    if let Some(poll_item) = app_state.polls.get_mut(&a) {
                                        if poll_item.polls.len() > poll_int.try_into().unwrap_or_default() {
                                            poll_item.poll(poll_int.try_into().unwrap_or_default());
                                            println!("Poll value: {}", poll_int);
                                        }
                                    }
                                }
                            } else {
                                println!("Poll value is not an integer");
                            }
                        } else {
                            println!("Poll field not found in JSON");
                        }
                    }
                    Err(e) => {
                        eprintln!("Error parsing JSON: {}", e);
                    }
                }

                if msg.is_close() {
                    break;
                }
            }
            Err(e) => {
                eprintln!("websocket Error(id={}): {}", id, e);
                break;
            }
        };
    }

    poll_disconnected(id, &app_state_clone).await;
}

async fn poll_message(msg: &str, app_state_clone: &AppStateArc) {
    for (&_uid, tx) in app_state_clone.read().await.poll_connections.iter() {
        if let Err(_disconnected) = tx.send(Message::text(msg)) {}
    }
}

async fn poll_disconnected(id: usize, app_state_clone: &AppStateArc) {
    println!("Poll Disconnected: {}", id);
    app_state_clone.write().await.poll_connections.remove(&id);
}
