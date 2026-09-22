use nom::{
    bytes::complete::{tag, take},
    number::complete::{be_u16, le_i16, le_u16, le_u32},
    IResult,
};
use serde::Serialize;

/// Inverter model, which selects the physical scaling of the payload fields.
///
/// The AC-power and daily-energy words use different scales per model. The
/// BDM-800 scales were calibrated on 2026-07-05 against a Shelly Outdoor
/// PlugS Gen3 reference meter the inverter feeds through (see CLAUDE.md):
/// the documented BDM-400 scales under-read a live BDM-800 by a flat x1.273
/// (power) / x1.155 (energy) across 20-620 W and three days of history.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
pub enum InverterModel {
    #[default]
    Bdm400,
    Bdm800,
    /// BDM-1200-LV and other newer firmware (WiFi fw 3.01.25+): posts a
    /// 69-byte payload to `/t.php` instead of the 45-byte `/i.php`. AC power
    /// scales /25.6 and AC voltage /51.2, validated against the NEP cloud's
    /// own reported power and the app's voltage on a 120 V leg (2026-09-22).
    Bdm1200Lv,
}

impl InverterModel {
    /// Lenient parse of a model name ("BDM-800", "bdm800", ...). Returns
    /// `None` for unrecognized names so callers can warn and pick a default.
    pub fn from_name(name: &str) -> Option<Self> {
        let n: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_uppercase();
        match n.as_str() {
            "BDM400" => Some(Self::Bdm400),
            "BDM800" => Some(Self::Bdm800),
            "BDM1200LV" | "BDM1200" => Some(Self::Bdm1200Lv),
            _ => None,
        }
    }

    /// Scales the raw AC-power word (bytes 25-26) to Watts.
    ///
    /// BDM-400: /100 (documented; internally consistent with its captures but
    /// never validated against a reference meter — it may share the BDM-800's
    /// 4/pi error, see CLAUDE.md).
    /// BDM-800: the Shelly-fitted correction is x1.2742 (weighted) / x1.2729
    /// (median), statistically indistinguishable from 4/pi = 1.27324, so the
    /// scale is taken as raw / (25*pi) ~= raw / 78.54.
    fn scale_power_w(self, raw: u16) -> f64 {
        match self {
            Self::Bdm400 => raw as f64 / 100.0,
            Self::Bdm800 => raw as f64 / (25.0 * std::f64::consts::PI),
            // Validated against the NEP cloud's own totalNow (263->302 W) on a
            // live BDM-1200-LV, 2026-09-22.
            Self::Bdm1200Lv => raw as f64 / 25.6,
        }
    }

    /// Scales the raw daily-energy word (bytes 37-38) to Watt-hours.
    ///
    /// BDM-400: 0.2 Wh per count (documented, same caveat as the power scale).
    /// BDM-800: 0.2308 Wh per count — pooled fit over four clean monotonic
    /// counter segments vs the Shelly reference (0.2301..0.2314, ~= 3/13).
    fn scale_energy_wh(self, raw: u16) -> f64 {
        match self {
            Self::Bdm400 => raw as f64 / 5.0,
            Self::Bdm800 => raw as f64 * 0.2308,
            // The BDM-1200-LV /t.php daily-energy word has not been located
            // yet (byte 37 is an upload counter, not energy). Unused.
            Self::Bdm1200Lv => raw as f64 / 5.0,
        }
    }

    /// Scales the raw AC-voltage word to Volts. The /i.php models use /25.6;
    /// the BDM-1200-LV /t.php voltage word (byte 53) uses /51.2 -- validated
    /// NOTE: on the BDM-1200-LV /t.php this byte-53 word is NOT grid RMS. A
    /// single 122.5 V match was coincidental; it tracks an internal/DC value
    /// that changes with the active PV input (inverse to string voltage), so
    /// treat it as a diagnostic pending further decode.
    fn scale_voltage_v(self, raw: u16) -> f64 {
        match self {
            Self::Bdm1200Lv => raw as f64 / 51.2,
            _ => raw as f64 / 25.6,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NepTelemetry {
    /// Model whose scales were used to decode this packet.
    pub model: InverterModel,
    pub serial_number: u32,
    pub status_code: u16,
    pub ac_power_w: f64,
    /// Decoded from bytes 27-28 at /25.6. CAUTION: on the BDM-800 this is an
    /// internal voltage measurement, NOT grid RMS voltage — it rests at
    /// V_peak/2 when idle and droops with output power when generating,
    /// matching grid voltage only around 400-550 W (see CLAUDE.md). The
    /// BDM-400's "grid voltage" interpretation is unconfirmed against a
    /// reference meter and likely has the same behaviour.
    pub ac_voltage_v: f64,
    pub operating_flags: u16,
    /// Total DC input current in Amperes (sum of both MPPT channels).
    pub dc_current_a: f64,
    /// DC input current, MPPT channel 1 (byte 31). Always 0 on single-input BDM-400.
    pub dc_current_ch1_a: f64,
    /// DC input current, MPPT channel 2 (byte 32). The single string on BDM-400.
    pub dc_current_ch2_a: f64,
    pub ac_freq_hz: f64,
    pub dc_voltage_v: f64,
    pub temp_c: f64,
    pub daily_energy_wh: f64,
    pub version: String,
    pub version_raw: u16,
    pub reactive_power_var: f64,
    /// Per-PV-input AC power in Watts (index 0 = input/addr 1 .. 2 = input/addr 3).
    /// Only populated for the BDM-1200-LV `/t.php` payload (offsets 43/49/55,
    /// 6-byte stride, each u16 LE /25.6); they sum to `ac_power_w`. Zeroed on
    /// the /i.php models. Validated 59/59 payloads on a live 3-input unit.
    pub pv_input_power_w: [f64; 3],
}

impl NepTelemetry {
    /// Decodes the w0 status code into a human-readable error state string.
    pub fn error_state_str(&self) -> &'static str {
        match self.status_code {
            0x0000 => "OK",
            0x0020 => "Grid Loss / Islanding",
            0x0004 => "Anti-Islanding Reconnection Sync Timer",
            _ => "Unknown Error",
        }
    }

    /// Decodes the w8 low byte into a human-readable operating mode string.
    ///
    /// The mode byte is model-specific: a live BDM-800 emits 0x05 while
    /// generating at full power (confirmed against a reference meter all day
    /// on 2026-07-05), whereas on the BDM-400 0x05 was only ever seen during
    /// a grid-event transition. The documented bit-mask reading (bit0 sleep /
    /// bit1 MPPT / bit2 relay) does not transfer across models.
    pub fn operating_mode_str(&self) -> &'static str {
        let w8_low = self.version_raw & 0xFF;
        match (self.model, w8_low) {
            (InverterModel::Bdm800, 0x05) => "Generating",
            (_, 0x01) => "Deep Sleep",
            (_, 0x04) => "Awake Standby",
            (_, 0x05) => "Active Standby",
            (_, 0x06) => "Generating",
            _ => "Unknown State",
        }
    }
}

/// Validates the dual checksums at the end of a 45-byte payload.
/// Checksum is calculated on bytes 1 to 42 (inclusive).
pub fn validate_checksums(data: &[u8]) -> bool {
    if data.len() < 45 {
        return false;
    }
    let body = &data[1..43];
    let sum: u8 = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let xor: u8 = body.iter().fold(0u8, |acc, &b| acc ^ b);
    data[43] == sum && data[44] == xor
}

fn parse_header(input: &[u8]) -> IResult<&[u8], ()> {
    // 0: Signature 'y' (0x79)
    let (input, _) = tag([0x79])(input)?;
    // 1-2: Length (0x0026 = 38 bytes)
    let (input, _) = tag([0x26, 0x00])(input)?;
    // 3-4: Cmd type (0x4014)
    let (input, _) = tag([0x40, 0x14])(input)?;
    // 5-12: Gateway / AP identifier (8 bytes). This is NOT a fixed constant.
    // The BDM-400 emits 0xFF padding here, but the BDM-800 emits a different
    // value (observed: `00 00 0f 0f 0f 0f 00 00`). The original
    // `tag([0xFF; 8])` therefore hard-rejected every non-BDM-400 device with
    // "Header parsing failed", even though the packet and its checksums were
    // valid. Skip the field instead of matching it. See CLAUDE.md.
    let (input, _) = take(8usize)(input)?;
    // 13-14: Data Length (0x001c = 28 bytes)
    let (input, _) = tag([0x1C, 0x00])(input)?;
    Ok((input, ()))
}

fn parse_data_section(input: &[u8], model: InverterModel) -> IResult<&[u8], NepTelemetry> {
    // 15-18: Sync marker. Usually 0xC3C3C3C3, but after an inverter reset a
    // live BDM-800 was observed emitting 0xFFFFFFFF here — presumably an
    // "uninitialized" placeholder, just like the 0xFF Gateway/AP field on the
    // BDM-400. The old exact match rejected such packets even though their
    // checksums were valid. Packet integrity is already guaranteed by the dual
    // checksums validated in parse_payload(), so skip the field instead of
    // matching it. See CLAUDE.md.
    let (input, _) = take(4usize)(input)?;
    // 19-22: Serial number
    let (input, serial_number) = le_u32(input)?;
    // 23-24: Status code
    let (input, status_code) = le_u16(input)?;
    // 25-26: AC Power
    let (input, ac_power_raw) = le_u16(input)?;
    // 27-28: voltage word — an internal measurement, not grid RMS (see the
    // ac_voltage_v field docs and CLAUDE.md).
    let (input, ac_voltage_raw) = le_u16(input)?;
    // 29-30: Operating flags
    let (input, operating_flags) = le_u16(input)?;
    // 31-32: DC Current (BE)
    let (input, dc_current_raw) = be_u16(input)?;
    // 33-34: AC Grid Frequency
    let (input, ac_freq_raw) = le_u16(input)?;
    // 35-36: Temperature
    let (input, temp_raw) = le_u16(input)?;
    // 37-38: Daily Energy
    let (input, daily_energy_raw) = le_u16(input)?;
    // 39-40: Version
    let (input, version_raw) = le_u16(input)?;
    // 41-42: Reactive Power (signed)
    let (input, reactive_power_raw) = le_i16(input)?;

    let ac_power_w = model.scale_power_w(ac_power_raw);
    let ac_voltage_v = ac_voltage_raw as f64 / 25.6;
    // DC input current. On the single-input BDM-400 the high byte (offset 31)
    // is always 0x00 and the low byte (offset 32) carries the single string's
    // current at /10 A, so the historical `be_u16 / 10` worked. The dual-MPPT
    // BDM-800 populates BOTH bytes (one per string), which made `be_u16 / 10`
    // explode to a nonsensical ~977 A.
    //
    // We instead decode the two bytes as independent per-string currents at
    // /10 A. This is exact for the BDM-400 (ch1 == 0, so the total equals the
    // old value and the existing unit tests still pass) and physically sane
    // for the BDM-800 (e.g. 3.8 A + 4.2 A = 8.0 A total).
    //
    // NOTE: the byte->channel mapping (31 = ch1, 32 = ch2) is a HYPOTHESIS
    // derived from a single BDM-800 packet — see CLAUDE.md. Treat `dc_current_a`
    // (the total) as the robust figure and confirm ch1/ch2 individually against
    // a capture series before relying on them.
    let dc_current_ch1_a = (dc_current_raw >> 8) as f64 / 10.0;
    let dc_current_ch2_a = (dc_current_raw & 0xFF) as f64 / 10.0;
    let dc_current_a = dc_current_ch1_a + dc_current_ch2_a;
    let ac_freq_hz = ac_freq_raw as f64 / 256.0;
    let temp_c = temp_raw as f64 / 100.0;
    let daily_energy_wh = model.scale_energy_wh(daily_energy_raw);
    let dc_voltage_v = if dc_current_a > 0.05 {
        (ac_power_w / (0.96 * dc_current_a)).min(60.0)
    } else {
        0.0
    };
    let reactive_power_var = reactive_power_raw as f64 / 100.0;
    let version = format!("{}.{:02}", version_raw / 100, version_raw % 100);

    Ok((
        input,
        NepTelemetry {
            model,
            serial_number,
            status_code,
            ac_power_w,
            ac_voltage_v,
            operating_flags,
            dc_current_a,
            dc_current_ch1_a,
            dc_current_ch2_a,
            ac_freq_hz,
            dc_voltage_v,
            temp_c,
            daily_energy_wh,
            version,
            version_raw,
            reactive_power_var,
            pv_input_power_w: [0.0, 0.0, 0.0],
        },
    ))
}

/// Parses the entire 45-byte payload, scaling fields for the given model.
pub fn parse_payload(input: &[u8], model: InverterModel) -> Result<NepTelemetry, String> {
    if input.len() < 45 {
        return Err(format!(
            "Payload too short: expected 45 bytes, got {}",
            input.len()
        ));
    }
    if !validate_checksums(input) {
        return Err("Checksum validation failed".to_string());
    }

    let (remaining, _) =
        parse_header(input).map_err(|e| format!("Header parsing failed: {:?}", e))?;
    let (_, telemetry) = parse_data_section(remaining, model)
        .map_err(|e| format!("Data section parsing failed: {:?}", e))?;
    Ok(telemetry)
}

/// Parse the 69-byte `/t.php` payload emitted by newer NEP firmware (e.g. the
/// BDM-1200-LV on WiFi fw 3.01.25+). Same `0x79`/`0x4014` framing and dual
/// checksums as the 45-byte `/i.php` payload, but with an extended 52-byte data
/// section. Only fields validated against the NEP cloud and app are decoded;
/// the per-input DC currents and the daily-energy word have not been located
/// yet and are reported as 0 (derive energy in Home Assistant by integrating
/// AC power).
///
/// Field map (offsets into the 69-byte frame), validated 2026-09:
/// * `19..23` u32 LE  serial number
/// * `23..25` u16 LE  status code
/// * `25..27` u16 LE  AC power   (`scale_power_w`  -> /25.6 W on BDM-1200-LV)
/// * `33..35` u16 LE  frequency  (/256 Hz)
/// * `35..37` u16 LE  DSP temperature (/100 C)
/// * `53..55` u16 LE  internal voltage (/51.2) -- NOT grid RMS; input-dependent
///
/// Byte 37 is an upload counter (not daily energy); bytes 55-56 duplicate the
/// power word and byte 57 duplicates byte 37.
pub fn parse_tphp_payload(input: &[u8], model: InverterModel) -> Result<NepTelemetry, String> {
    if input.len() < 69 {
        return Err(format!(
            "Payload too short: expected 69 bytes, got {}",
            input.len()
        ));
    }
    let data = &input[..69];
    if data[0] != 0x79 {
        return Err(format!("Bad start byte: 0x{:02x}", data[0]));
    }
    // Dual checksums over bytes 1..67 (additive sum + XOR), stored at 67/68 --
    // the same scheme as /i.php, just at the extended frame's offsets.
    let body = &data[1..67];
    let sum: u8 = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let xor: u8 = body.iter().fold(0u8, |acc, &b| acc ^ b);
    if data[67] != sum || data[68] != xor {
        return Err("Checksum validation failed".to_string());
    }

    let u16le = |o: usize| (data[o] as u16) | ((data[o + 1] as u16) << 8);
    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let serial_number = (data[19] as u32)
        | ((data[20] as u32) << 8)
        | ((data[21] as u32) << 16)
        | ((data[22] as u32) << 24);

    Ok(NepTelemetry {
        model,
        serial_number,
        status_code: u16le(23),
        ac_power_w: round2(model.scale_power_w(u16le(25))),
        ac_voltage_v: round2(model.scale_voltage_v(u16le(53))),
        operating_flags: u16le(29),
        dc_current_a: 0.0,
        dc_current_ch1_a: 0.0,
        dc_current_ch2_a: 0.0,
        ac_freq_hz: round2(u16le(33) as f64 / 256.0),
        dc_voltage_v: 0.0,
        temp_c: round2(u16le(35) as f64 / 100.0),
        daily_energy_wh: 0.0,
        version: String::new(),
        version_raw: u16le(39),
        reactive_power_var: 0.0,
        pv_input_power_w: [
            round2(model.scale_power_w(u16le(43))),
            round2(model.scale_power_w(u16le(49))),
            round2(model.scale_power_w(u16le(55))),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tphp_bdm1200lv() {
        // Live BDM-1200-LV /t.php packet, serial 0x86513AE0.
        let hex = "793e004014ffffffffffffffff3400c3c3c3c3e03a51860000431a3f00700800010e3c600e120005eb3f00000000007f00000000002018431a120000000000000061d5256b";
        let bytes = hex::decode(hex).unwrap();
        let t = parse_tphp_payload(&bytes, InverterModel::Bdm1200Lv).unwrap();
        assert_eq!(t.serial_number, 0x86513ae0);
        assert_eq!(t.ac_power_w, 262.62); // 6723 / 25.6
        assert_eq!(t.ac_voltage_v, 120.63); // 6176 / 51.2 (round half away from zero)
        assert_eq!(t.ac_freq_hz, 60.05); // 15374 / 256
        assert_eq!(t.temp_c, 36.8); // 3680 / 100
        // Single input active (PV3/addr3 @55); inputs 1 and 2 idle. The three
        // per-input powers sum to the total AC power.
        assert_eq!(t.pv_input_power_w, [0.0, 0.0, 262.62]);
        let sum: f64 = t.pv_input_power_w.iter().sum();
        assert!((sum - t.ac_power_w).abs() < 0.05, "per-input sum {sum} vs total {}", t.ac_power_w);
    }

    #[test]
    fn test_parse_tphp_second_sample() {
        let hex = "793e004014ffffffffffffffff3400c3c3c3c3e03a51860000f41c4f0060080001fc3b510f370005eb4f0000000000800000000000dc17f41c3700000000000000b9dbde64";
        let bytes = hex::decode(hex).unwrap();
        let t = parse_tphp_payload(&bytes, InverterModel::Bdm1200Lv).unwrap();
        assert_eq!(t.serial_number, 0x86513ae0);
        assert_eq!(t.ac_power_w, 289.53); // 7412 / 25.6
        assert_eq!(t.ac_voltage_v, 119.3); // 6108 / 51.2
        assert_eq!(t.ac_freq_hz, 59.98); // 15356 / 256
        assert_eq!(t.temp_c, 39.21); // 3921 / 100
    }

    #[test]
    fn test_parse_tphp_rejects_bad_checksum() {
        let mut bytes = hex::decode("793e004014ffffffffffffffff3400c3c3c3c3e03a51860000431a3f00700800010e3c600e120005eb3f00000000007f00000000002018431a120000000000000061d5256b").unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff; // corrupt XOR checksum
        assert!(parse_tphp_payload(&bytes, InverterModel::Bdm1200Lv).is_err());
    }

    #[test]
    fn test_parse_tphp_rejects_short() {
        assert!(parse_tphp_payload(&[0x79; 45], InverterModel::Bdm1200Lv).is_err());
    }

    #[test]
    fn test_inverter_model_from_name_bdm1200lv() {
        assert_eq!(InverterModel::from_name("BDM-1200-LV"), Some(InverterModel::Bdm1200Lv));
        assert_eq!(InverterModel::from_name("bdm1200"), Some(InverterModel::Bdm1200Lv));
    }

    #[test]
    fn test_parse_payload1() {
        let hex = "7926004014ffffffffffffffff1c00c3c3c3c3785634120000755752172010002bfe31cf145c1a0684bb2e395f";
        let bytes = hex::decode(hex).unwrap();
        assert!(validate_checksums(&bytes));
        let telemetry = parse_payload(&bytes, InverterModel::Bdm400).unwrap();
        assert_eq!(telemetry.serial_number, 0x12345678);
        assert_eq!(telemetry.ac_power_w, 223.89);
        assert_eq!(telemetry.ac_voltage_v, 233.203125); // 5970 / 25.6
        assert_eq!(telemetry.ac_freq_hz, 49.9921875); // 12798 / 256
        assert_eq!(telemetry.temp_c, 53.27);
        assert_eq!(telemetry.daily_energy_wh, 1349.6);
    }

    #[test]
    fn test_parse_payload6() {
        let hex = "7926004014ffffffffffffffff1c00c3c3c3c3785634120000443552183010001af8312f136a21068402026f5b";
        let bytes = hex::decode(hex).unwrap();
        assert!(validate_checksums(&bytes));
        let telemetry = parse_payload(&bytes, InverterModel::Bdm400).unwrap();
        assert_eq!(telemetry.serial_number, 0x12345678);
        assert_eq!(telemetry.ac_power_w, 136.36);
        assert_eq!(telemetry.dc_current_a, 2.6);
        assert_eq!(telemetry.reactive_power_var, 5.14);
        assert_eq!(telemetry.temp_c, 49.11);
        assert_eq!(telemetry.daily_energy_wh, 1710.8);
    }

    #[test]
    fn test_parse_payload_bdm800() {
        // NEP BDM-800 packet captured from a live unit. The serial number has
        // been anonymized to 0xDEADBEEF (bytes 19..22) with both checksums
        // recomputed; every other byte is as captured.
        // Distinguishing features vs the BDM-400 fixtures above:
        //   - Gateway/AP field (bytes 5..12) is `00 00 0f 0f 0f 0f 00 00`, NOT
        //     0xFF padding. The old strict `tag([0xFF; 8])` rejected this packet.
        //   - Both DC-current bytes (31, 32) are populated (dual-MPPT), so the
        //     old `be_u16 / 10` produced ~977 A. Per-channel decode fixes it.
        let hex = "792600401400000f0f0f0f00001c00c3c3c3c3efbeadde0000bd56cb172010262ab0317013ab12058c05e02270";
        let bytes = hex::decode(hex).unwrap();
        assert!(validate_checksums(&bytes));
        let t = parse_payload(&bytes, InverterModel::Bdm800).unwrap();

        // AC power and daily energy use the Shelly-calibrated BDM-800 scales
        // (raw * 4/(100*pi) W and raw * 0.2308 Wh); the other fields decode
        // identically to the BDM-400 layout.
        assert_eq!(t.serial_number, 0xdeadbeef);
        assert!((t.ac_power_w - 282.72).abs() < 0.01); // 22205 * 4/(100*pi)
        assert_eq!(t.ac_freq_hz, 49.6875); // 12720 / 256
        assert!((t.ac_voltage_v - 237.93).abs() < 0.01); // 6091 / 25.6
        assert!((t.daily_energy_wh - 1102.99).abs() < 0.01); // 4779 * 0.2308
        // Mode byte 0x05 means "generating" on the BDM-800 (confirmed against
        // a reference meter), not the BDM-400's "Active Standby".
        assert_eq!(t.operating_mode_str(), "Generating");

        // DC-side: two per-string currents that sum to a physically sane total,
        // instead of the ~977 A the single-BE-u16 interpretation produced.
        assert_eq!(t.dc_current_ch1_a, 3.8); // byte 31 = 0x26 = 38 -> /10
        assert_eq!(t.dc_current_ch2_a, 4.2); // byte 32 = 0x2a = 42 -> /10
        assert_eq!(t.dc_current_a, 8.0);
    }

    #[test]
    fn test_parse_payload_bdm800_post_reset_sync_marker() {
        // Captured from the same live BDM-800 on the first report after the
        // inverter was reset (serial anonymized to 0xDEADBEEF, checksums
        // recomputed; every other byte as captured). Bytes 15..19 — normally
        // the 0xC3C3C3C3 sync marker — read 0xFFFFFFFF here, which the old
        // exact match rejected despite both checksums validating.
        let hex = "792600401400000f0f0f0f00001c00ffffffffefbeadde0000b8ae581650104a55a631cf149104058c94cd1a42";
        let bytes = hex::decode(hex).unwrap();
        assert!(validate_checksums(&bytes));
        let t = parse_payload(&bytes, InverterModel::Bdm800).unwrap();

        assert_eq!(t.serial_number, 0xdeadbeef);
        assert!((t.ac_power_w - 569.49).abs() < 0.01); // 44728 * 4/(100*pi)
        assert!((t.ac_voltage_v - 223.44).abs() < 0.01); // 5720 / 25.6
        assert_eq!(t.ac_freq_hz, 49.6484375); // 12710 / 256
        assert_eq!(t.temp_c, 53.27);
        assert!((t.daily_energy_wh - 269.81).abs() < 0.01); // 1169 * 0.2308; low: reset cleared the accumulator
        assert_eq!(t.dc_current_ch1_a, 7.4); // byte 31 = 0x4a = 74 -> /10
        assert_eq!(t.dc_current_ch2_a, 8.5); // byte 32 = 0x55 = 85 -> /10
        assert_eq!(t.reactive_power_var, -129.08);
    }

    #[test]
    fn test_parse_payload_rejects_bad_checksums() {
        // Corrupting any body byte must fail checksum validation, now the only
        // integrity guard for the relaxed gateway/AP and sync-marker fields.
        let hex = "792600401400000f0f0f0f00001c00ffffffffefbeadde0000b8ae581650104a55a631cf149104058c94cd1a42";
        let mut bytes = hex::decode(hex).unwrap();
        bytes[25] ^= 0x01;
        assert!(!validate_checksums(&bytes));
        assert_eq!(
            parse_payload(&bytes, InverterModel::Bdm800).unwrap_err(),
            "Checksum validation failed"
        );
    }

    #[test]
    fn test_inverter_model_from_name() {
        assert_eq!(InverterModel::from_name("BDM-800"), Some(InverterModel::Bdm800));
        assert_eq!(InverterModel::from_name("bdm800"), Some(InverterModel::Bdm800));
        assert_eq!(InverterModel::from_name("BDM 400"), Some(InverterModel::Bdm400));
        assert_eq!(InverterModel::from_name("BDM-600"), None);
    }
}
