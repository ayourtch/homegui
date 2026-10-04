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
use handlebars::{handlebars_helper, Handlebars};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tide_handlebars::prelude::*;

use std::time::SystemTime;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    mqtthost: String,
    client_name: Option<String>,
    actions: HashMap<String, HashMap<String, Vec<(String, String)>>>,
    #[serde(default)]
    autodim: AutoDimConfig,
}

/// Evening auto-dim: once a day, at or after `time` (local), every light that is ON
/// in one of `rooms` and brighter than `brightness_percent` is dimmed to it.
/// It never raises a light that is already dimmer. If the time is missed (service
/// down, restarted late), it still runs later the same day: `flag_file` holds the
/// date of the last run, so it runs at most once per day across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoDimConfig {
    enabled: bool,
    /// "HH:MM", local time of this machine
    time: String,
    brightness_percent: u8,
    /// room = first word of the friendly name ("Living Window - 0x..." is in "Living")
    rooms: Vec<String>,
    flag_file: String,
}

impl Default for AutoDimConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: "22:00".to_string(),
            brightness_percent: 30,
            rooms: vec!["Living".to_string(), "Kitchen".to_string()],
            flag_file: "autodim-last-run".to_string(),
        }
    }
}

/// "HH:MM" -> minutes since midnight.
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// zigbee2mqtt brightness is 0..=254.
fn percent_to_level(percent: u8) -> u64 {
    (percent.min(100) as u64 * 254 + 50) / 100
}

/// Local date ("YYYY-MM-DD") and minutes since midnight, from the system clock and
/// time zone (via `date`, so no time-zone crate is needed).
fn local_now() -> Option<(String, u32)> {
    let out = std::process::Command::new("date")
        .arg("+%F %H:%M")
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let (date, time) = text.trim().split_once(' ')?;
    Some((date.to_string(), parse_hhmm(time)?))
}

/// Due when it is at or past the dim time and the last run was not today.
fn autodim_due(today: &str, now_min: u32, at_min: u32, last_run: Option<&str>) -> bool {
    now_min >= at_min && last_run.map(str::trim) != Some(today)
}

/// Lights to dim: in one of `rooms`, reported ON, and brighter than `level`.
/// A light whose state or brightness is not known yet is left alone.
fn autodim_targets(
    devices: &HashMap<String, RenderDeviceEntry>,
    rooms: &[String],
    level: u64,
) -> Vec<String> {
    let mut names: Vec<String> = devices
        .iter()
        .filter(|(_, dev)| rooms.iter().any(|r| r == &dev.room_name))
        .filter(|(_, dev)| {
            let payload: Value = match serde_json::from_str(&dev.last_payload) {
                Ok(v) => v,
                Err(_) => return false,
            };
            payload["state"] == "ON"
                && payload["brightness"].as_u64().map_or(false, |b| b > level)
        })
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

fn autodim_status(cfg: &AutoDimConfig, done_today: bool) -> String {
    if !cfg.enabled {
        return "Auto-dim is off".to_string();
    }
    format!(
        "Auto-dim at {} to {}% ({}): {}",
        cfg.time,
        cfg.brightness_percent,
        cfg.rooms.join(", "),
        if done_today { "done today" } else { "not yet today" }
    )
}

async fn autodim_loop(state: Arc<Mutex<AyTestState>>, client: AsyncClient, cfg: AutoDimConfig) {
    let at = match parse_hhmm(&cfg.time) {
        Some(at) => at,
        None => {
            println!("autodim: cannot parse time {:?}; auto-dim disabled", cfg.time);
            state.lock().await.data.autodim_status =
                format!("Auto-dim is off (bad time {:?} in config)", cfg.time);
            return;
        }
    };
    let level = percent_to_level(cfg.brightness_percent);
    let started = std::time::Instant::now();
    // Also remembered in memory: if the flag file cannot be written, it must not
    // re-dim every 30 s for the rest of the evening.
    let mut ran_on: Option<String> = None;
    loop {
        let Some((today, now)) = local_now() else {
            println!("autodim: cannot read the local time");
            task::sleep(Duration::from_secs(30)).await;
            continue;
        };
        let last_run = std::fs::read_to_string(&cfg.flag_file).ok();
        let done_today = ran_on.as_deref() == Some(today.as_str())
            || last_run.as_deref().map(str::trim) == Some(today.as_str());
        let mut st = state.lock().await;
        st.data.autodim_status = autodim_status(&cfg, done_today);
        // Wait a minute after start so the lights have reported their state; the
        // device list must be in too.
        let ready = started.elapsed() >= Duration::from_secs(60) && !st.data.devices.is_empty();
        if ready && !done_today && autodim_due(&today, now, at, last_run.as_deref()) {
            let names = autodim_targets(&st.data.devices, &cfg.rooms, level);
            let payload = format!("{{ \"brightness\": {} }}", level);
            for name in &names {
                if let Some(dev) = st.data.devices.get_mut(name) {
                    dev.last_req_sent = SystemTime::now();
                }
                let target = format!("zigbee2mqtt/{}/set", name);
                let client = client.clone();
                let payload = payload.clone();
                task::spawn(async move {
                    if let Err(e) = client
                        .publish(&target, QoS::AtMostOnce, false, payload.as_bytes())
                        .await
                    {
                        println!("autodim: publish to {} failed: {:?}", target, e);
                    }
                });
            }
            println!("autodim: {} dimmed {} light(s) to {}: {:?}", today, names.len(), level, names);
            ran_on = Some(today.clone());
            let tmp = format!("{}.tmp", &cfg.flag_file);
            if let Err(e) = std::fs::write(&tmp, format!("{}\n", today))
                .and_then(|_| std::fs::rename(&tmp, &cfg.flag_file))
            {
                println!("autodim: cannot write flag file {}: {:?}", &cfg.flag_file, e);
            }
            st.data.autodim_status = autodim_status(&cfg, true);
        }
        drop(st);
        task::sleep(Duration::from_secs(30)).await;
    }
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
    let last_req_sent = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(1000))
        .unwrap();
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
    autodim_status: String,
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
                autodim_status: String::new(),
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
    println!("Change state for {}: {}", &name, &update);

    let payload = update.clone();
    let target = format!("zigbee2mqtt/{}/set", &name);
    let mut state = req.state().lock().await;
    if let Some(dev) = state.data.devices.get_mut(&name) {
        dev.last_req_sent = SystemTime::now();
        println!("Set req sent to: {:?}", &dev.last_req_sent);
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
    let client_name = config.client_name.unwrap_or("homegui-rs".to_string());

    let mut mqttoptions = MqttOptions::new(&client_name, config.mqtthost, 1883);
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

    state.data.autodim_status = autodim_status(&config.autodim, false);
    let mut state = Arc::new(Mutex::new(state));
    let mut iot_state = state.clone();

    if config.autodim.enabled {
        task::spawn(autodim_loop(state.clone(), client.clone(), config.autodim.clone()));
    }

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
        let notification = match eventloop.poll().await {
            Ok(n) => n,
            Err(e) => {
                println!("MQTT connection error: {:?}; reconnecting in 2s", e);
                async_std::task::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        println!("Received = {:?}", notification);
        match notification {
            rumqttc::Event::Incoming(incoming) => match incoming {
                rumqttc::Packet::ConnAck(_) => {
                    println!("MQTT (re)connected; subscribing");
                    let _ = client
                        .subscribe("zigbee2mqtt/bridge/Xlogging", QoS::AtMostOnce)
                        .await;
                    let _ = client
                        .subscribe("zigbee2mqtt/bridge/devices", QoS::AtMostOnce)
                        .await;
                    let _ = client
                        .subscribe("zigbee2mqtt/+", QoS::AtMostOnce)
                        .await;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(friendly: &str, payload: &str) -> (String, RenderDeviceEntry) {
        let entry = DeviceEntry {
            ieee_address: "0x1".into(),
            typ: "Router".into(),
            network_address: 1,
            supported: true,
            friendly_name: friendly.into(),
        };
        let mut d = get_render_device(&entry).expect("a light name");
        d.last_payload = payload.into();
        (friendly.to_string(), d)
    }

    #[test]
    fn hhmm() {
        assert_eq!(parse_hhmm("22:00"), Some(1320));
        assert_eq!(parse_hhmm(" 7:05 "), Some(425));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("22"), None);
    }

    #[test]
    fn percent_scale() {
        assert_eq!(percent_to_level(30), 76);
        assert_eq!(percent_to_level(100), 254);
        assert_eq!(percent_to_level(0), 0);
        assert_eq!(percent_to_level(250), 254);
    }

    #[test]
    fn due_once_a_day_and_catches_up() {
        let at = 22 * 60;
        assert!(!autodim_due("2026-10-04", 21 * 60 + 59, at, None), "not before the time");
        assert!(autodim_due("2026-10-04", 22 * 60, at, None), "at the time");
        assert!(autodim_due("2026-10-04", 23 * 60 + 30, at, Some("2026-10-03\n")), "late, missed today");
        assert!(!autodim_due("2026-10-04", 23 * 60, at, Some("2026-10-04\n")), "already ran today");
        assert!(!autodim_due("2026-10-05", 30, at, Some("2026-10-04")), "after midnight: next day, before time");
    }

    #[test]
    fn targets_only_on_brighter_lights_in_the_rooms() {
        let devices: HashMap<String, RenderDeviceEntry> = [
            dev("Living Window - 0x01", r#"{"state":"ON","brightness":200}"#),
            dev("Living Above Couch - 0x02", r#"{"state":"ON","brightness":40}"#),
            dev("Kitchen 1 - 0x03", r#"{"brightness":254,"state":"ON"}"#),
            dev("Kitchen 2 - 0x04", r#"{"state":"OFF","brightness":254}"#),
            dev("Kitchen Plug - 0x05", r#"{"state":"ON"}"#),
            dev("Kitchen 3 - 0x06", ""),
            dev("BBe Top 1 - 0x07", r#"{"state":"ON","brightness":254}"#),
        ]
        .into_iter()
        .collect();
        let rooms = vec!["Living".to_string(), "Kitchen".to_string()];
        assert_eq!(
            autodim_targets(&devices, &rooms, 76),
            vec!["Kitchen 1 - 0x03".to_string(), "Living Window - 0x01".to_string()]
        );
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c: Config = toml::from_str("mqtthost = \"x\"\n[actions]\n").unwrap();
        assert!(c.autodim.enabled);
        assert_eq!(c.autodim.time, "22:00");
        assert_eq!(c.autodim.brightness_percent, 30);
        let c: Config = toml::from_str(
            "mqtthost = \"x\"\n[actions]\n[autodim]\ntime = \"21:30\"\nbrightness_percent = 20\n",
        )
        .unwrap();
        assert_eq!((c.autodim.time.as_str(), c.autodim.brightness_percent), ("21:30", 20));
        assert_eq!(c.autodim.rooms, vec!["Living", "Kitchen"], "unset keys keep defaults");
    }
}
