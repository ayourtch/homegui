use async_std::task;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
    /// "HH:MM" in `timezone`
    time: String,
    /// TZ name for `time`; belair-living itself runs on UTC
    timezone: String,
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
            timezone: "Europe/Brussels".to_string(),
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

/// Date ("YYYY-MM-DD") and minutes since midnight in time zone `tz` (via `date`
/// with TZ set, so no time-zone crate is needed).
fn local_now(tz: &str) -> Option<(String, u32)> {
    let out = std::process::Command::new("date")
        .env("TZ", tz)
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

/// Lights to dim: in one of `rooms`, online, reported ON, and brighter than `level`.
/// A light whose state or brightness is not known yet is left alone.
fn autodim_targets(
    devices: &HashMap<String, RenderDeviceEntry>,
    rooms: &[String],
    level: u64,
) -> Vec<String> {
    let mut names: Vec<String> = devices
        .iter()
        .filter(|(_, dev)| rooms.iter().any(|r| r == &dev.room_name))
        // An offline light keeps its last report; a command to it stalls zigbee2mqtt.
        .filter(|(_, dev)| dev.available)
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

/// True when every light in `rooms` has reported a state since start or is offline.
/// Dimming before that would miss lights and still mark the day done. An offline
/// light never answers the startup "get" (measured 2026-10-04: the 6 lights that
/// never reported were exactly the 6 zigbee2mqtt marks offline).
fn room_states_known(devices: &HashMap<String, RenderDeviceEntry>, rooms: &[String]) -> bool {
    devices
        .values()
        .filter(|dev| rooms.iter().any(|r| r == &dev.room_name))
        .all(|dev| !dev.last_payload.is_empty() || !dev.available)
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

/// Night mode: the evening dim, forced or undone from the page. `saved` holds the
/// brightness each dimmed light had before, for the undo. Memory only: after a
/// restart the undo is lost and the lights stay as they are.
#[derive(Debug, Clone, Default)]
struct NightMode {
    saved: Vec<(String, u64)>,
}

fn payload_of(dev: &RenderDeviceEntry) -> Value {
    serde_json::from_str(&dev.last_payload).unwrap_or(Value::Null)
}

/// Lights to dim, each with its brightness now (for the undo).
fn night_plan(
    devices: &HashMap<String, RenderDeviceEntry>,
    rooms: &[String],
    level: u64,
) -> Vec<(String, u64)> {
    autodim_targets(devices, rooms, level)
        .into_iter()
        .filter_map(|name| {
            let b = payload_of(devices.get(&name)?)["brightness"].as_u64()?;
            Some((name, b))
        })
        .collect()
}

/// Lights to put back on undo: still ON and still at the dimmed level (a bulb may
/// report a step off), so a light changed by hand since is left alone.
fn restore_plan(
    saved: &[(String, u64)],
    devices: &HashMap<String, RenderDeviceEntry>,
    level: u64,
) -> Vec<(String, u64)> {
    saved
        .iter()
        .filter(|(name, _)| {
            devices.get(name).map_or(false, |dev| {
                let p = payload_of(dev);
                dev.available
                    && p["state"] == "ON"
                    && p["brightness"].as_u64().map_or(false, |b| b.abs_diff(level) <= 2)
            })
        })
        .cloned()
        .collect()
}

fn publish_set(st: &mut AyTestState, name: &str, payload: String) {
    if let Some(dev) = st.data.devices.get_mut(name) {
        dev.last_req_sent = SystemTime::now();
    }
    let target = format!("zigbee2mqtt/{}/set", name);
    let client = st.client.clone();
    task::spawn(async move {
        if let Err(e) = client
            .publish(&target, QoS::AtMostOnce, false, payload.as_bytes())
            .await
        {
            println!("publish to {} failed: {:?}", target, e);
        }
    });
}

/// Record that the dim ran today, in memory and in the flag file (atomic write).
/// In memory too: if the file cannot be written it must not re-dim every 30 s.
fn mark_ran_today(st: &mut AyTestState, today: &str) {
    st.ran_on = Some(today.to_string());
    let flag = st.autodim.flag_file.clone();
    let tmp = format!("{}.tmp", &flag);
    if let Err(e) = std::fs::write(&tmp, format!("{}\n", today)).and_then(|_| std::fs::rename(&tmp, &flag)) {
        println!("autodim: cannot write flag file {}: {:?}", &flag, e);
    }
    st.data.autodim_status = autodim_status(&st.autodim, true);
}

/// Dim now. Also counts as today's run, so the automatic dim does not follow.
fn night_on(st: &mut AyTestState, today: &str) -> Vec<String> {
    let level = percent_to_level(st.autodim.brightness_percent);
    let plan = night_plan(&st.data.devices, &st.autodim.rooms, level);
    for (name, _) in &plan {
        publish_set(st, name, format!("{{ \"brightness\": {} }}", level));
    }
    let night = st.night.get_or_insert_with(NightMode::default);
    for (name, b) in &plan {
        if !night.saved.iter().any(|(n, _)| n == name) {
            night.saved.push((name.clone(), *b));
        }
    }
    st.data.night_mode = true;
    mark_ran_today(st, today);
    plan.into_iter().map(|(name, _)| name).collect()
}

/// Undo: put the dimmed lights back. Today stays marked as done.
fn night_off(st: &mut AyTestState) -> Vec<String> {
    st.data.night_mode = false;
    let Some(night) = st.night.take() else {
        return vec![];
    };
    let level = percent_to_level(st.autodim.brightness_percent);
    let plan = restore_plan(&night.saved, &st.data.devices, level);
    for (name, b) in &plan {
        publish_set(st, name, format!("{{ \"brightness\": {} }}", b));
    }
    plan.into_iter().map(|(name, _)| name).collect()
}

async fn autodim_loop(state: Arc<Mutex<AyTestState>>) {
    let cfg = state.lock().await.autodim.clone();
    let at = match parse_hhmm(&cfg.time) {
        Some(at) => at,
        None => {
            println!("autodim: cannot parse time {:?}; auto-dim disabled", cfg.time);
            state.lock().await.data.autodim_status =
                format!("Auto-dim is off (bad time {:?} in config)", cfg.time);
            return;
        }
    };
    let started = std::time::Instant::now();
    loop {
        let Some((today, now)) = local_now(&cfg.timezone) else {
            println!("autodim: cannot read the local time");
            task::sleep(Duration::from_secs(30)).await;
            continue;
        };
        let last_run = std::fs::read_to_string(&cfg.flag_file).ok();
        let mut st = state.lock().await;
        let done_today = st.ran_on.as_deref() == Some(today.as_str())
            || last_run.as_deref().map(str::trim) == Some(today.as_str());
        st.data.autodim_status = autodim_status(&cfg, done_today);
        // Wait for the device list and for the lights to report their state. If
        // availability is not published, give up waiting after 10 minutes and dim
        // the ones that did report.
        let waited = started.elapsed();
        let ready = !st.data.devices.is_empty()
            && waited >= Duration::from_secs(5)
            && (room_states_known(&st.data.devices, &cfg.rooms)
                || waited >= Duration::from_secs(600));
        if ready && !done_today && autodim_due(&today, now, at, last_run.as_deref()) {
            let names = night_on(&mut st, &today);
            println!("autodim: {} dimmed {} light(s): {:?}", today, names.len(), names);
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
    /// zigbee2mqtt availability; an offline light never answers a "get"
    available: bool,
    available_since: SystemTime,
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
        available: true,
        available_since: SystemTime::UNIX_EPOCH,
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
    night_mode: bool,
}

#[derive(Clone)]
struct AyTestState {
    tempdir: Arc<TempDir>,
    registry: Handlebars<'static>,
    client: rumqttc::AsyncClient,
    data: RenderData,
    autodim: AutoDimConfig,
    /// date the dim last ran (auto or forced); also in `autodim.flag_file`
    ran_on: Option<String>,
    night: Option<NightMode>,
    /// availability seen before the device list arrived (retained messages come first)
    offline: HashSet<String>,
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
                night_mode: false,
            },
            autodim: AutoDimConfig::default(),
            ran_on: None,
            night: None,
            offline: HashSet::new(),
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
    if state.data.devices.get(&name).map_or(false, |dev| !dev.available) {
        // A light switched off at the wall: a command to it stalls zigbee2mqtt.
        println!("Device {} is offline, not sending", &name);
        return Ok(format!("{} is offline", name).into());
    }
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
        if dev.available && dev.last_req_sent < dev.last_payload_update {
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
struct NightModeArgs {
    on: bool,
}

async fn set_night_mode(mut req: Request<Arc<Mutex<AyTestState>>>) -> tide::Result {
    let NightModeArgs { on } = req.body_json().await?;
    let tz = req.state().lock().await.autodim.timezone.clone();
    let mut st = req.state().lock().await;
    let names = if on {
        let Some((today, _)) = local_now(&tz) else {
            return Err(tide::Error::from_str(500, "cannot read the local time"));
        };
        night_on(&mut st, &today)
    } else {
        night_off(&mut st)
    };
    println!("night mode {}: {:?}", if on { "on" } else { "off" }, names);
    Ok(json!({ "night_mode": st.data.night_mode, "lights": names }).into())
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
        if (dev.last_payload_update > last_update)
            || (dev.last_req_sent > last_update)
            || (dev.available_since > last_update)
        {
            out.push(dev.clone());
        }
    }

    Ok(json!({
        "devices": out,
        "last_update": &new_last_update,
        "night_mode": state.data.night_mode,
    })
    .into())
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
    client
        .subscribe("zigbee2mqtt/+/availability", QoS::AtMostOnce)
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
    state.autodim = config.autodim.clone();
    let mut state = Arc::new(Mutex::new(state));
    let mut iot_state = state.clone();

    if config.autodim.enabled {
        task::spawn(autodim_loop(state.clone()));
    }

    let mut app = tide::with_state(state);
    app.at("/").get(root_req);
    app.at("/static/").serve_dir("static/").unwrap();
    app.at("/set-state").post(set_state);
    app.at("/set-all-off").post(set_all_off);
    app.at("/get-state").post(get_state);
    app.at("/night-mode").post(set_night_mode);

    {
        let client = client.clone();
        task::spawn(async move {
            // HOMEGUI_LISTEN lets a test copy run beside the live one.
            let listen =
                std::env::var("HOMEGUI_LISTEN").unwrap_or_else(|_| "0.0.0.0:8989".to_string());
            app.listen(listen).await.unwrap();
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
                    let _ = client
                        .subscribe("zigbee2mqtt/+/availability", QoS::AtMostOnce)
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
                            if let Some(mut render_device) = get_render_device(&d) {
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
                                    render_device.available = !state.offline.contains(&friendly_name);
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
                    } else if let Some(name) = publish
                        .topic
                        .strip_prefix("zigbee2mqtt/")
                        .and_then(|k| k.strip_suffix("/availability"))
                    {
                        // {"state":"online"}, or plain "online" from older zigbee2mqtt
                        let s = String::from_utf8_lossy(&publish.payload);
                        let online = serde_json::from_str::<Value>(&s)
                            .ok()
                            .and_then(|v| v["state"].as_str().map(|x| x == "online"))
                            .unwrap_or(s.trim() == "online");
                        let state = &mut iot_state.lock().await;
                        if online {
                            state.offline.remove(name);
                        } else {
                            state.offline.insert(name.to_string());
                        }
                        if let Some(dev) = state.data.devices.get_mut(name) {
                            dev.available = online;
                            dev.available_since = SystemTime::now();
                        }
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
    fn now_follows_the_configured_zone() {
        let (_, utc) = local_now("UTC").unwrap();
        let (_, kiri) = local_now("Pacific/Kiritimati").unwrap(); // UTC+14, no DST
        assert_eq!((utc + 14 * 60) % (24 * 60), kiri);
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
            dev("Living At Target - 0x08", r#"{"state":"ON","brightness":76}"#),
            dev("Living Just Above - 0x09", r#"{"state":"ON","brightness":77}"#),
            dev("Kitchen No State - 0x0a", r#"{"brightness":200}"#),
            dev("Kitchen Offline - 0x0b", r#"{"state":"ON","brightness":254}"#),
        ]
        .into_iter()
        .collect();
        let mut devices = devices;
        devices.get_mut("Kitchen Offline - 0x0b").unwrap().available = false;
        let rooms = vec!["Living".to_string(), "Kitchen".to_string()];
        assert_eq!(
            autodim_targets(&devices, &rooms, 76),
            vec![
                "Kitchen 1 - 0x03".to_string(),
                "Living Just Above - 0x09".to_string(),
                "Living Window - 0x01".to_string()
            ]
        );
    }

    #[test]
    fn waits_for_room_lights_only() {
        let rooms = vec!["Living".to_string()];
        let mut devices: HashMap<String, RenderDeviceEntry> = [
            dev("Living Window - 0x01", r#"{"state":"ON","brightness":200}"#),
            dev("Kitchen 1 - 0x03", ""),
        ]
        .into_iter()
        .collect();
        assert!(room_states_known(&devices, &rooms), "an unknown light in another room does not block");
        devices.extend([dev("Living Lamp - 0x02", "")]);
        assert!(!room_states_known(&devices, &rooms), "an unknown light in the room blocks");
        devices.get_mut("Living Lamp - 0x02").unwrap().available = false;
        assert!(room_states_known(&devices, &rooms), "an offline light does not block");
    }

    #[test]
    fn night_plan_saves_brightness_and_undo_skips_changed_lights() {
        let rooms = vec!["Living".to_string()];
        let before: HashMap<String, RenderDeviceEntry> = [
            dev("Living A - 0x01", r#"{"state":"ON","brightness":200}"#),
            dev("Living B - 0x02", r#"{"state":"ON","brightness":254}"#),
            dev("Living C - 0x03", r#"{"state":"ON","brightness":180}"#),
            dev("Living D - 0x04", r#"{"state":"ON","brightness":50}"#),
        ]
        .into_iter()
        .collect();
        let saved = night_plan(&before, &rooms, 76);
        let mut sorted = saved.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                ("Living A - 0x01".to_string(), 200),
                ("Living B - 0x02".to_string(), 254),
                ("Living C - 0x03".to_string(), 180)
            ]
        );
        let after: HashMap<String, RenderDeviceEntry> = [
            dev("Living A - 0x01", r#"{"state":"ON","brightness":77}"#),
            dev("Living B - 0x02", r#"{"state":"OFF","brightness":76}"#),
            dev("Living C - 0x03", r#"{"state":"ON","brightness":150}"#),
            dev("Living D - 0x04", r#"{"state":"ON","brightness":50}"#),
        ]
        .into_iter()
        .collect();
        let mut off_wall = after.clone();
        off_wall.get_mut("Living A - 0x01").unwrap().available = false;
        assert!(restore_plan(&saved, &off_wall, 76).is_empty(), "an offline light is not restored");
        assert_eq!(
            restore_plan(&saved, &after, 76),
            vec![("Living A - 0x01".to_string(), 200)],
            "only A is still on at the dimmed level"
        );
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c: Config = toml::from_str("mqtthost = \"x\"\n[actions]\n").unwrap();
        assert!(c.autodim.enabled);
        assert_eq!(c.autodim.time, "22:00");
        assert_eq!(c.autodim.timezone, "Europe/Brussels");
        assert_eq!(c.autodim.brightness_percent, 30);
        let c: Config = toml::from_str(
            "mqtthost = \"x\"\n[actions]\n[autodim]\ntime = \"21:30\"\nbrightness_percent = 20\n",
        )
        .unwrap();
        assert_eq!((c.autodim.time.as_str(), c.autodim.brightness_percent), ("21:30", 20));
        assert_eq!(c.autodim.rooms, vec!["Living", "Kitchen"], "unset keys keep defaults");
    }
}
