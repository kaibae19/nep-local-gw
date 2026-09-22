use nep_protocol::{InverterModel, NepTelemetry};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    /// `(username, password)` for brokers requiring authentication.
    pub credentials: Option<(String, String)>,
    /// Inverter model name shown in Home Assistant (e.g. "BDM-400", "BDM-800").
    pub model: String,
}

/// Dynamic MQTT Worker task using rumqttc
pub async fn run_mqtt_worker(config: MqttConfig, mut rx: mpsc::Receiver<NepTelemetry>) {
    let mqtt_host = config.host;
    let mqtt_port = config.port;

    info!(
        "Initializing MQTT Client connecting to {}:{}...",
        mqtt_host, mqtt_port
    );
    let mut mqttoptions = MqttOptions::new("nep-gateway", mqtt_host, mqtt_port);
    mqttoptions.set_keep_alive(std::time::Duration::from_secs(5));
    if let Some((username, password)) = config.credentials {
        info!("Using MQTT authentication as user '{}'", username);
        mqttoptions.set_credentials(username, password);
    }

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    // Spawn a connection monitoring task
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    tracing::trace!("MQTT Event: {:?}", notification);
                }
                Err(e) => {
                    warn!("MQTT Connection error: {:?}. Retrying in 5 seconds...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    });

    let mut discovered = false;

    while let Some(telemetry) = rx.recv().await {
        let serial = format!("{:08x}", telemetry.serial_number);

        // Run Home Assistant MQTT Discovery once on first telemetry capture
        if !discovered {
            info!(
                "Publishing Home Assistant MQTT Discovery configs for serial {}...",
                serial
            );
            if let Err(e) =
                publish_ha_discovery(&client, &serial, &config.model, telemetry.model).await
            {
                error!("HA Discovery publish failed: {:?}", e);
            } else {
                discovered = true;
            }
        }

        let dc_power = telemetry.dc_voltage_v * telemetry.dc_current_a;
        let efficiency = if dc_power > 0.0 {
            (telemetry.ac_power_w / dc_power * 100.0).min(100.0)
        } else {
            0.0
        };

        // Publish parsed values in a single clean JSON payload
        let telemetry_payload = json!({
            "ac_power_w": (telemetry.ac_power_w * 100.0).round() / 100.0,
            "ac_voltage_v": (telemetry.ac_voltage_v * 100.0).round() / 100.0,
            "ac_freq_hz": (telemetry.ac_freq_hz * 100.0).round() / 100.0,
            "dc_voltage_v": (telemetry.dc_voltage_v * 100.0).round() / 100.0,
            "dc_current_a": (telemetry.dc_current_a * 100.0).round() / 100.0,
            "dc_current_ch1_a": (telemetry.dc_current_ch1_a * 100.0).round() / 100.0,
            "dc_current_ch2_a": (telemetry.dc_current_ch2_a * 100.0).round() / 100.0,
            "dc_power_w": (dc_power * 100.0).round() / 100.0,
            "efficiency_percent": (efficiency * 100.0).round() / 100.0,
            "temperature_c": (telemetry.temp_c * 100.0).round() / 100.0,
            "daily_energy_wh": (telemetry.daily_energy_wh * 100.0).round() / 100.0,
            "reactive_power_var": (telemetry.reactive_power_var * 100.0).round() / 100.0,
            "pv1_power_w": (telemetry.pv_input_power_w[0] * 100.0).round() / 100.0,
            "pv2_power_w": (telemetry.pv_input_power_w[1] * 100.0).round() / 100.0,
            "pv3_power_w": (telemetry.pv_input_power_w[2] * 100.0).round() / 100.0,
            "status": if telemetry.status_code == 0 { "OK" } else { "Error" },
            "error_state": telemetry.error_state_str(),
            "operating_mode": telemetry.operating_mode_str(),
            "version": telemetry.version,
        });

        let topic = format!("nep/telemetry/{}", serial);
        info!("Publishing telemetry JSON to MQTT topic: {}", topic);
        if let Err(e) = client
            .publish(
                topic,
                QoS::AtLeastOnce,
                false,
                telemetry_payload.to_string(),
            )
            .await
        {
            error!("MQTT publish telemetry failed: {:?}", e);
        }
    }
}

async fn publish_ha_discovery(
    client: &AsyncClient,
    serial: &str,
    model: &str,
    model_kind: InverterModel,
) -> Result<(), rumqttc::ClientError> {
    let device = json!({
        "identifiers": [format!("nep_bdm_{}", serial)],
        "name": format!("NEP {} ({})", model, serial),
        "model": model,
        "manufacturer": "Northern Electric Power (NEP)"
    });

    let state_topic = format!("nep/telemetry/{}", serial);

    // Bytes 27-28 turned out to be an internal voltage measurement, not grid
    // RMS voltage: on a live BDM-800 it rests at V_peak/2 when idle and
    // droops with output power when generating, only coincidentally matching
    // grid voltage around 400-550 W (see CLAUDE.md). Label it honestly on the
    // BDM-800; the BDM-400 keeps its historical name pending confirmation
    // against a reference meter. The unique_id stays "ac_voltage" either way
    // so existing HA entities and their history are preserved.
    let ac_voltage_label = match model_kind {
        InverterModel::Bdm800 => "Internal Bus Voltage",
        InverterModel::Bdm400 => "AC Grid Voltage",
        // On the BDM-1200-LV the /t.php voltage word IS real grid RMS
        // (validated against the app on a 120 V leg, 2026-09-22).
        InverterModel::Bdm1200Lv => "AC Grid Voltage",
    };

    let mut sensors = vec![
        ("ac_power", "AC Output Power", "power", "W", "ac_power_w"),
        (
            "ac_voltage",
            ac_voltage_label,
            "voltage",
            "V",
            "ac_voltage_v",
        ),
        (
            "ac_freq",
            "AC Grid Frequency",
            "frequency",
            "Hz",
            "ac_freq_hz",
        ),
        (
            "dc_voltage",
            "DC PV Voltage",
            "voltage",
            "V",
            "dc_voltage_v",
        ),
        (
            "dc_current",
            "DC PV Current",
            "current",
            "A",
            "dc_current_a",
        ),
        (
            "dc_current_ch1",
            "DC PV Current CH1",
            "current",
            "A",
            "dc_current_ch1_a",
        ),
        (
            "dc_current_ch2",
            "DC PV Current CH2",
            "current",
            "A",
            "dc_current_ch2_a",
        ),
        ("dc_power", "DC PV Power", "power", "W", "dc_power_w"),
        (
            "efficiency",
            "Inverter Efficiency",
            "",
            "%",
            "efficiency_percent",
        ),
        (
            "temperature",
            "DSP Temperature",
            "temperature",
            "°C",
            "temperature_c",
        ),
        (
            "daily_energy",
            "Daily Energy",
            "energy",
            "Wh",
            "daily_energy_wh",
        ),
        (
            "reactive_power",
            "Reactive Power",
            "reactive_power",
            "VAR",
            "reactive_power_var",
        ),
        ("error_state", "Error State", "enum", "", "error_state"),
        (
            "operating_mode",
            "Operating Mode",
            "enum",
            "",
            "operating_mode",
        ),
    ];

    // BDM-1200-LV (/t.php): only AC power, voltage, frequency, temperature and
    // error state are decoded, plus per-input AC power (offsets 43/49/55 summing
    // to the total). Drop the DC-side / daily-energy / reactive / operating-mode
    // placeholders that aren't located for this payload, and add PV 1/2/3.
    if model_kind == InverterModel::Bdm1200Lv {
        sensors.retain(|(id, ..)| {
            matches!(*id, "ac_power" | "ac_voltage" | "ac_freq" | "temperature" | "error_state")
        });
        sensors.push(("pv1_power", "PV Input 1 Power", "power", "W", "pv1_power_w"));
        sensors.push(("pv2_power", "PV Input 2 Power", "power", "W", "pv2_power_w"));
        sensors.push(("pv3_power", "PV Input 3 Power", "power", "W", "pv3_power_w"));
    }

    for (id, name, dev_class, unit, json_key) in sensors {
        let mut config = json!({
            "name": format!("NEP {} {}", serial, name),
            "state_topic": state_topic,
            "value_template": format!("{{{{ value_json.{} }}}}", json_key),
            "unique_id": format!("nep_{}_{}", serial, id),
            "device": device
        });

        if !dev_class.is_empty() {
            config["device_class"] = json!(dev_class);
        }
        if !unit.is_empty() {
            config["unit_of_measurement"] = json!(unit);
        }

        match id {
            "daily_energy" => config["state_class"] = json!("total_increasing"),
            "efficiency" => {}
            "error_state" => {
                config["options"] = json!([
                    "OK",
                    "Grid Loss / Islanding",
                    "Anti-Islanding Reconnection Sync Timer",
                    "Unknown Error"
                ]);
            }
            "operating_mode" => {
                config["options"] = json!([
                    "Deep Sleep",
                    "Awake Standby",
                    "Active Standby",
                    "Generating",
                    "Unknown State"
                ]);
            }
            _ => config["state_class"] = json!("measurement"),
        }

        let discovery_topic = format!("homeassistant/sensor/nep_{}/{}/config", serial, id);
        client
            .publish(discovery_topic, QoS::AtLeastOnce, true, config.to_string())
            .await?;
    }

    Ok(())
}
