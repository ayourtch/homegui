use async_std::task;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::time::Duration;

use http_types::mime;
use tide::prelude::*;
use tide::{Body, Request, Response, StatusCode};

use async_std::sync::Arc;
use async_std::sync::Mutex;
use handlebars::{handlebars_helper, Handlebars, JsonRender};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tide_handlebars::prelude::*;

use std::time::SystemTime;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    mqtthost: String,
    actions: HashMap<String, HashMap<String, Vec<(String, String)>>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceEvent {
    action: Option<String>,
    linkquality: Option<u8>,
    battery: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceDefinition {
    model: String,
    vendor: String,
    description: String,
    // option: ...
    // exposes: ...
    //
    //
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceEntry {
    ieee_address: String,
    #[serde(rename = "type")]
    typ: String,
    network_address: u32,
    supported: bool,
    // disabled: bool,
    friendly_name: String,
    // description: String,
    // endpoints: Vec<...>,
    // definition: DeviceDefinition,
    // power_source: String,
    // date_code: String,
    // model_id: String,
    // scenes:
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenderDeviceEntry {
    device: DeviceEntry,
    short_name: String,
    room_name: String,
    html_id: String,
    last_payload: String,
    last_payload_update: SystemTime,
    last_req_sent: SystemTime,
}

fn get_render_device(d: &DeviceEntry) -> Option<RenderDeviceEntry> {
    let names = vec!["Switch", "Router", "Coordinator", "sensor"];
    for name in names {
        if d.friendly_name.contains(name) {
            return None;
        }
    }
    let d = d.clone();
    let parts: Vec<&str> = d.friendly_name.split("- 0x").collect();
    if parts.len() < 2 {
        println!("Could not split {}, not inserting", &d.friendly_name);
        return None;
    }
    let mut spsp = parts[0].split(" ");
    let room_name = spsp.next().unwrap_or("none").to_string();
    let short_name = spsp
        .map(|x| x.to_string())
        .collect::<Vec<String>>()
        .join(" ")
        .trim()
        .to_string();
    let html_id = d.friendly_name.replace(" ", "_");
    let device = d;
    let last_payload = format!("");
    let last_payload_update = SystemTime::now();
    let last_req_sent = last_payload_update;
    Some(RenderDeviceEntry {
        device,
        room_name,
        short_name,
        html_id,
        last_payload,
        last_payload_update,
        last_req_sent,
    })
}

#[derive(Clone, Serialize)]
struct RoomRenderData {
    device_names: Vec<String>,
}

#[derive(Clone, Serialize)]
struct RenderData {
    devices: HashMap<String, RenderDeviceEntry>,
    rooms: HashMap<String, RoomRenderData>,
}

#[derive(Clone)]
struct AyTestState {
    tempdir: Arc<TempDir>,
    registry: Handlebars<'static>,
    client: rumqttc::AsyncClient,
    data: RenderData,
}

handlebars_helper!(devicealive: |dev: RenderDeviceEntry| dev.last_payload_update > dev.last_req_sent );

impl AyTestState {
    fn new(client: rumqttc::AsyncClient) -> Self {
        let mut hb = Handlebars::new();
        hb.register_helper("devicealive", Box::new(devicealive));
        Self {
            tempdir: Arc::new(tempfile::tempdir().unwrap()),
            registry: hb,
            client,
            data: RenderData {
                rooms: HashMap::new(),
                devices: HashMap::new(),
            },
        }
    }

    fn path(&self) -> &Path {
        self.tempdir.path()
    }
}

#[derive(Deserialize)]
struct RequestQuery {
    url: String,
}

#[derive(Debug, Deserialize)]
struct Device {
    name: String,
    update: String,
}

async fn set_state(mut req: Request<Arc<Mutex<AyTestState>>>) -> tide::Result {
    let Device { name, update } = req.body_json().await?;
    println!("Change state for {}", &name);

    let payload = update.clone();
    let target = format!("zigbee2mqtt/{}/set", &name);
    let mut state = req.state().lock().await;
    if let Some(dev) = state.data.devices.get_mut(&name) {
        dev.last_req_sent = SystemTime::now();
        let client = state.client.clone();
        task::spawn(async move {
            client
                .publish(&target, QoS::AtMostOnce, false, payload.as_bytes())
                .await
                .unwrap();
        });
    } else {
        println!("Device {} not known!", &name);
    }

    Ok(format!("I've changed the state for {} ", name).into())
}

async fn set_all_off(mut req: Request<Arc<Mutex<AyTestState>>>) -> tide::Result {
    let mut state = req.state().lock().await;
    let client = state.client.clone();
    for (name, ref mut dev) in &mut state.data.devices {
        if dev.last_req_sent < dev.last_payload_update {
            let name = format!("{}", &dev.device.friendly_name);
            let payload = format!("{{ \"state\": \"{}\" }}", "OFF");
            let target = format!("zigbee2mqtt/{}/set", &name);
            dev.last_req_sent = SystemTime::now();
            let client = client.clone();
            task::spawn(async move {
                client
                    .publish(&target, QoS::AtMostOnce, false, payload.as_bytes())
                    .await
                    .unwrap();
            });
        }
    }
    Ok(format!("All off!").into())
}

#[derive(Debug, Deserialize)]
struct GetStateArgs {
    last_update: SystemTime,
}

async fn get_state(mut req: Request<Arc<Mutex<AyTestState>>>) -> tide::Result {
    let GetStateArgs { last_update } = req.body_json().await.unwrap_or(GetStateArgs {
        last_update: SystemTime::UNIX_EPOCH,
    });
    println!("get state since {:?}", &last_update);
    let state = req.state().lock().await;
    let mut out: Vec<RenderDeviceEntry> = vec![];
    let new_last_update = SystemTime::now();

    for (name, dev) in &state.data.devices {
        if (dev.last_payload_update > last_update) || (dev.last_req_sent > last_update) {
            out.push(dev.clone());
        }
    }

    Ok(json!({ "devices": out, "last_update": &new_last_update }).into())
}

async fn root_req(mut req: Request<Arc<Mutex<AyTestState>>>) -> tide::Result {
    use std::collections::BTreeMap;

    // let RequestQuery { url } = req.query().unwrap();
    /*
    let mut res: surf::Response = surf::get(url).await?;
    let data: String = res.body_string().await?;
    */

    let state = req.state().lock().await;
    let hb = &state.registry;
    let data0 = &state.data;
    let body = hb.render("index.html", &data0)?;
    let mut response = Response::builder(200)
        .body(body)
        .header("custom-header", "value")
        .content_type(mime::HTML)
        .build();
    Ok(response)
}

#[async_std::main]
async fn main() {
    dotenv::dotenv().ok();
    let mut cfg: Config = Default::default();

    let mut act: Vec<(String, String)> = Default::default();

    act.push(("asd".to_string(), r#"{ "state": "TOGGLE" }"#.to_string()));
    act.push(("dedas".to_string(), r#"{"state": "ON"}"#.to_string()));

    let mut map1: HashMap<String, Vec<(String, String)>> = Default::default();

    map1.insert("action_push".to_string(), act);
    cfg.actions.insert("switch1 - test".to_string(), map1);
    println!("Config: {}", toml::to_string(&cfg).unwrap());

    let cfg = std::fs::read_to_string("config.toml").unwrap();

    let config: Config = toml::from_str(&cfg).unwrap();

    println!("Config: {:?}", &config);

    let mut mqttoptions = MqttOptions::new("homegui-rs", config.mqtthost, 1883);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    mqttoptions.set_max_packet_size(1000000, 1000000);

    let (mut client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    client
        .subscribe("zigbee2mqtt/bridge/Xlogging", QoS::AtMostOnce)
        .await
        .unwrap();
    client
        .subscribe("zigbee2mqtt/bridge/devices", QoS::AtMostOnce)
        .await
        .unwrap();
    client
        .subscribe("zigbee2mqtt/+", QoS::AtMostOnce)
        .await
        .unwrap();

    /*
     * let json_bytes: Vec<u8> = r#"{"brightness":56,"color":{"x":0.46187,"y":0.19485},"color_mode":"xy","color_temp":250,"state":"ON"}"#.into();
    client
        .publish(
            "zigbee2mqtt/Living Above Couch - 0x000b57fffea0074a/set",
            QoS::AtMostOnce,
            false,
            json_bytes,
        )
        .await
        .unwrap();

    */

    tide::log::start();
    let mut state = AyTestState::new(client.clone());
    state.registry.set_dev_mode(true);
    state
        .registry
        .register_templates_directory("", "./templates/")
        .unwrap();

    let mut state = Arc::new(Mutex::new(state));
    let mut iot_state = state.clone();

    let mut app = tide::with_state(state);
    app.at("/").get(root_req);
    app.at("/static/").serve_dir("static/").unwrap();
    app.at("/set-state").post(set_state);
    app.at("/set-all-off").post(set_all_off);
    app.at("/get-state").post(get_state);

    {
        let client = client.clone();
        task::spawn(async move {
            app.listen("0.0.0.0:8989").await.unwrap();
        });
    }

    loop {
        let notification = eventloop.poll().await.unwrap();
        println!("Received = {:?}", notification);
        match notification {
            rumqttc::Event::Incoming(incoming) => match incoming {
                rumqttc::Packet::Publish(publish) => {
                    if publish.topic == "zigbee2mqtt/bridge/devices" {
                        let devices: Vec<DeviceEntry> =
                            serde_json::from_slice(&publish.payload).unwrap();

                        println!("Devices:");
                        let mut delay_count = 1;

                        for d in &devices {
                            let state = &mut iot_state.lock().await;
                            if let Some(render_device) = get_render_device(&d) {
                                println!("Insert: {:?}", &render_device);
                                let friendly_name = render_device.device.friendly_name.clone();
                                let has_key = { state.data.devices.get(&friendly_name).is_some() };
                                let room = state
                                    .data
                                    .rooms
                                    .entry(render_device.room_name.clone())
                                    .or_insert_with(|| RoomRenderData {
                                        device_names: vec![],
                                    });
                                if !has_key {
                                    room.device_names.push(friendly_name.clone());
                                    state.data.devices.insert(friendly_name, render_device);

                                    let payload = format!("{{ \"state\": \"\" }}");
                                    let target = format!("zigbee2mqtt/{}/get", &d.friendly_name);

                                    let client = client.clone();
                                    task::spawn(async move {
                                        async_std::task::sleep(Duration::from_millis(
                                            50 * delay_count,
                                        ))
                                        .await;
                                        client
                                            .publish(
                                                &target,
                                                QoS::AtMostOnce,
                                                false,
                                                payload.as_bytes(),
                                            )
                                            .await
                                            .unwrap();
                                    });
                                    delay_count += 1;
                                }
                            }

                            println!("{}", &d.friendly_name);
                            /*
                            if !d.friendly_name.starts_with("Living") {
                                println!("Subscribe {}", &d.friendly_name);
                                client
                                    .subscribe(
                                        &format!("zigbee2mqtt/{}", &d.friendly_name),
                                        QoS::AtMostOnce,
                                    )
                                    .await
                                    .unwrap();
                            }
                            */
                        }
                        println!("------");
                    } else {
                        let key = publish.topic.clone().replace("zigbee2mqtt/", "");
                        if let Some(cfg) = config.actions.get(&key) {
                            println!("Found key {}", &key);
                            let s = String::from_utf8_lossy(&publish.payload);
                            println!("payload: {}", s);
                            let event: DeviceEvent =
                                serde_json::from_slice(&publish.payload).unwrap();
                            println!("Event: {:?}", &event);
                            if let Some(action) = event.action {
                                if let Some(actions) = cfg.get(&action) {
                                    println!(
                                        "Found list of actions for event {}: {:?}",
                                        &action, &actions
                                    );
                                    for (dev, payload) in actions {
                                        let client = client.clone();
                                        let payload = payload.clone().replace("'", r#"""#);
                                        let target = format!("zigbee2mqtt/{}/set", dev);
                                        task::spawn(async move {
                                            client
                                                .publish(
                                                    &target,
                                                    QoS::AtMostOnce,
                                                    false,
                                                    payload.as_bytes(),
                                                )
                                                .await
                                                .unwrap();
                                        });
                                    }
                                }
                            }
                        } else {
                            println!("publish: {:?}", &publish);
                            let s = String::from_utf8_lossy(&publish.payload);
                            if s.contains("\"state\":") {
                                println!("STATE payload: {}", s);
                                let mut state = &mut iot_state.lock().await;
                                if let Some(ref mut dev) = state.data.devices.get_mut(&key) {
                                    println!("Device {} found!", &key);
                                    dev.last_payload = s.to_string();
                                    dev.last_payload_update = SystemTime::now();
                                }
                            } else {
                                println!("payload: {}", s);
                            }
                        }
                    }
                }
                x => {
                    println!("all other incoming: {:?}", &x);
                }
            },
            x => {
                println!("All other notification: {:?}", &x);
            }
        }
    }
}
