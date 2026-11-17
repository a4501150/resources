use std::{
    fmt::Display,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use glob::glob;
use lazy_regex::{Lazy, Regex, lazy_regex};
use log::{debug, trace};

use crate::utils::i18n::i18n;
use crate::utils::{FLATPAK_APP_PATH, FLATPAK_SPAWN, IS_FLATPAK, read_parsed};

const PROC_MEMINFO: &str = "/proc/meminfo";

/// hwmon drivers that expose on-DIMM thermal sensors (TSOD)
const KNOWN_RAM_HWMON_DRIVERS: &[&str] = &["spd5118", "jc42", "ee1004"];

const TEMPLATE_RE_PRESENT: &str = r"MEMORY_DEVICE_%_PRESENT=(\d)";

const TEMPLATE_RE_CONFIGURED_SPEED_MTS: &str = r"MEMORY_DEVICE_%_CONFIGURED_SPEED_MTS=(\d*)";

const TEMPLATE_RE_SPEED_MTS: &str = r"MEMORY_DEVICE_%_SPEED_MTS=(\d*)";

const TEMPLATE_RE_FORM_FACTOR: &str = r"MEMORY_DEVICE_%_FORM_FACTOR=(.*)";

const TEMPLATE_RE_TYPE: &str = r"MEMORY_DEVICE_%_TYPE=(.*)";

const TEMPLATE_RE_TYPE_DETAIL: &str = r"MEMORY_DEVICE_%_TYPE_DETAIL=(.*)";

const TEMPLATE_RE_SIZE: &str = r"MEMORY_DEVICE_%_SIZE=(\d*)";

const BYTES_IN_GIB: u64 = 1_073_741_824; // 1024 * 1024 * 1024

static RE_CONFIGURED_SPEED: Lazy<Regex> = lazy_regex!(r"Configured Memory Speed: (\d+) MT/s");

static RE_SPEED: Lazy<Regex> = lazy_regex!(r"Speed: (\d+) MT/s");

static RE_FORMFACTOR: Lazy<Regex> = lazy_regex!(r"Form Factor: (.+)");

static RE_TYPE: Lazy<Regex> = lazy_regex!(r"Type: (.+)");

static RE_TYPE_DETAIL: Lazy<Regex> = lazy_regex!(r"Type Detail: (.+)");

static RE_SIZE: Lazy<Regex> = lazy_regex!(r"Size: (\d+) GB");

static RE_MEM_TOTAL: Lazy<Regex> = lazy_regex!(r"MemTotal:\s*(\d*) kB");

static RE_MEM_AVAILABLE: Lazy<Regex> = lazy_regex!(r"MemAvailable:\s*(\d*) kB");

static RE_PROC_SWAPS_LINE: Lazy<Regex> = lazy_regex!(r"^(\S+)\s+(\S+)\s+(\d+)\s+(\d+)\s+(-?\d+)$");

static RE_NUM_MEMORY_DEVICES: Lazy<Regex> = lazy_regex!(r"MEMORY_ARRAY_NUM_DEVICES=(\d*)");

#[derive(Debug, Clone)]
pub struct MemoryData {
    pub total_mem: usize,
    pub available_mem: usize,
    pub swap_devices: Vec<SwapDevice>,
    /// Per-module RAM temperatures in °C (`None` if that sensor could not be
    /// read this time), empty if no TSOD sensor is readable
    pub ram_temperatures: Vec<Option<f32>>,
}

/// How often to look for RAM temperature sensors again while none is
/// readable, e. g. because `spd5118` got loaded after the app started
const RAM_SENSOR_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

/// hwmon `temp*_input` paths of every detected RAM module thermal sensor and
/// the time they were searched for
static RAM_TEMPERATURE_PATHS: LazyLock<Mutex<(Vec<PathBuf>, Option<Instant>)>> =
    LazyLock::new(|| Mutex::new((Vec::new(), None)));

fn temp_input_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("temp"))
        .and_then(|name| name.strip_suffix("_input"))
        .and_then(|index| index.parse().ok())
        .unwrap_or(u32::MAX)
}

fn find_ram_temperature_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for hwmon in glob("/sys/class/hwmon/hwmon*")
        .expect("Invalid glob pattern")
        .flatten()
    {
        if let Ok(name) = read_parsed::<String>(hwmon.join("name"))
            && KNOWN_RAM_HWMON_DRIVERS.contains(&name.trim())
        {
            let mut temps = glob(&format!("{}/temp*_input", hwmon.to_string_lossy()))
                .expect("Invalid glob pattern")
                .flatten()
                .collect::<Vec<PathBuf>>();
            temps.sort_by_key(|path| temp_input_index(path));
            paths.extend(temps);
        }
    }
    if paths.is_empty() {
        debug!("No RAM module temperature sensors found!");
    }
    paths
}

fn read_ram_temperatures(paths: &[PathBuf]) -> Vec<Option<f32>> {
    paths
        .iter()
        .map(|path| {
            read_parsed::<u64>(path)
                .ok()
                .map(|millidegrees| millidegrees as f32 / 1000.0)
        })
        .collect()
}

/// Per-module RAM temperatures in °C, one entry per known sensor
pub fn ram_temperatures() -> Vec<Option<f32>> {
    let mut cache = RAM_TEMPERATURE_PATHS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (paths, last_scan) = &mut *cache;

    let mut temperatures = read_ram_temperatures(paths);
    if temperatures.iter().all(Option::is_none)
        && last_scan.is_none_or(|time| time.elapsed() >= RAM_SENSOR_RESCAN_INTERVAL)
    {
        *paths = find_ram_temperature_paths();
        *last_scan = Some(Instant::now());
        temperatures = read_ram_temperatures(paths);
    }

    if temperatures.iter().all(Option::is_none) {
        Vec::new()
    } else {
        temperatures
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapDevice {
    pub name: String,
    pub r#type: SwapType,
    pub size: usize,
    pub used: usize,
    pub priority: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SwapType {
    Partition,
    File,
    #[default]
    Unknown,
}

impl SwapType {
    pub fn from_proc_swaps<S: AsRef<str>>(swap_type: S) -> Self {
        match swap_type.as_ref() {
            "partition" => Self::Partition,
            "file" => Self::File,
            _ => Self::Unknown,
        }
    }
}

impl Display for SwapType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Partition => i18n("Partition"),
                Self::File => i18n("File"),
                Self::Unknown => i18n("N/A"),
            }
        )
    }
}

impl MemoryData {
    pub fn new() -> Result<Self> {
        trace!("Gathering memory data…");

        trace!("Reading {PROC_MEMINFO}…");
        let proc_mem = read_parsed::<String>("/proc/meminfo")?;

        let total_mem = RE_MEM_TOTAL
            .captures(&proc_mem)
            .context("RE_MEM_TOTAL no captures")
            .and_then(|captures| {
                captures
                    .get(1)
                    .context("RE_MEM_TOTAL not enough captures")
                    .and_then(|capture| {
                        capture
                            .as_str()
                            .parse::<usize>()
                            .context("unable to parse MemTotal")
                            .map(|int| int.saturating_mul(1024))
                    })
            })?;

        let available_mem = RE_MEM_AVAILABLE
            .captures(&proc_mem)
            .context("RE_MEM_AVAILABLE no captures")
            .and_then(|captures| {
                captures
                    .get(1)
                    .context("RE_MEM_AVAILABLE not enough captures")
                    .and_then(|capture| {
                        capture
                            .as_str()
                            .parse::<usize>()
                            .context("unable to parse MemAvailable")
                            .map(|int| int.saturating_mul(1024))
                    })
            })?;

        let memory_data = Self {
            total_mem,
            available_mem,
            swap_devices: SwapDevice::get_all().unwrap_or_default(),
            ram_temperatures: ram_temperatures(),
        };

        trace!("Gathered memory data: {memory_data:?}");

        Ok(memory_data)
    }
}

impl SwapDevice {
    pub fn get_all() -> Result<Vec<Self>> {
        trace!("Reading /proc/swaps…");
        let swaps_content = read_parsed::<String>("/proc/swaps")?;
        let mut devices = Vec::new();

        for line in swaps_content.lines().skip(1) {
            if let Some(captures) = RE_PROC_SWAPS_LINE.captures(line) {
                let name = captures[1].to_string();
                let r#type = SwapType::from_proc_swaps(&captures[2]);
                let size = captures[3].parse::<usize>()? * 1024;
                let used = captures[4].parse::<usize>()? * 1024;
                let priority = captures[5].parse::<i32>()?;

                devices.push(Self {
                    name,
                    r#type,
                    size,
                    used,
                    priority,
                });
            }
        }

        Ok(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proc_swaps() {
        let content = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                       /dev/dm-0                               partition\t15624188\t0\t-1\n\
                       /dev/zram0                              partition\t7782396\t664\t100\n";

        let mut devices = Vec::new();
        for line in content.lines().skip(1) {
            if let Some(captures) = RE_PROC_SWAPS_LINE.captures(line) {
                let name = captures[1].to_string();
                let r#type = SwapType::from_proc_swaps(&captures[2]);
                let size = captures[3].parse::<usize>().unwrap() * 1024;
                let used = captures[4].parse::<usize>().unwrap() * 1024;
                let priority = captures[5].parse::<i32>().unwrap();

                devices.push(SwapDevice {
                    name,
                    r#type,
                    size,
                    used,
                    priority,
                });
            }
        }

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "/dev/dm-0");
        assert_eq!(devices[0].r#type, SwapType::Partition);
        assert_eq!(devices[0].priority, -1);
        assert_eq!(devices[1].name, "/dev/zram0");
        assert_eq!(devices[1].used, 664 * 1024);
        assert_eq!(devices[1].r#type, SwapType::Partition);
        assert_eq!(devices[1].priority, 100);
    }

    #[test]
    fn swap_type_from_proc_swaps_maps_known_and_unknown_values() {
        assert_eq!(SwapType::from_proc_swaps("partition"), SwapType::Partition);
        assert_eq!(SwapType::from_proc_swaps("file"), SwapType::File);
        assert_eq!(SwapType::from_proc_swaps("unknown"), SwapType::Unknown);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryDevice {
    pub speed_mts: Option<u32>,
    pub form_factor: Option<String>,
    pub r#type: Option<String>,
    pub type_detail: Option<String>,
    pub size: Option<u64>,
    pub installed: bool,
}

impl MemoryDevice {
    fn parse_dmidecode<S: AsRef<str>>(dmi: S) -> Vec<Self> {
        trace!("Parsing dmidecode output…");
        let mut devices = Vec::new();

        let device_strings = dmi.as_ref().split("\n\n");

        for device_string in device_strings {
            if device_string.is_empty() {
                continue;
            }

            let memory_device = Self {
                speed_mts: RE_CONFIGURED_SPEED
                    .captures(device_string)
                    .or_else(|| RE_SPEED.captures(device_string))
                    .map(|x| x[1].parse().unwrap()),
                form_factor: RE_FORMFACTOR
                    .captures(device_string)
                    .map(|x| x[1].to_string()),
                r#type: RE_TYPE.captures(device_string).map(|x| x[1].to_string()),
                type_detail: RE_TYPE_DETAIL
                    .captures(device_string)
                    .map(|x| x[1].to_string()),
                size: RE_SIZE
                    .captures(device_string)
                    .map(|x| x[1].parse::<u64>().unwrap() * BYTES_IN_GIB),
                installed: RE_SPEED
                    .captures(device_string)
                    .map(|x| x[1].to_string())
                    .is_some(),
            };

            trace!("Found memory device: {memory_device:?}");

            devices.push(memory_device);
        }

        devices
    }

    fn virtual_dmi() -> Vec<Self> {
        let command = if *IS_FLATPAK {
            trace!("Executing udevadm outside Flatpak's sandbox…");
            Command::new(FLATPAK_SPAWN)
                .args([
                    "--host",
                    "udevadm",
                    "info",
                    "-p",
                    "/sys/devices/virtual/dmi/id",
                ])
                .output()
        } else {
            trace!("Executing udevadm…");
            Command::new("udevadm")
                .args(["info", "-p", "/sys/devices/virtual/dmi/id"])
                .output()
        };

        let virtual_dmi_output = command
            .context("unable to execute udevadm")
            .and_then(|output| {
                String::from_utf8(output.stdout)
                    .context("unable to parse stdout of udevadm to UTF-8")
            })
            .unwrap_or_default();

        Self::parse_virtual_dmi(virtual_dmi_output)
    }

    fn parse_virtual_dmi<S: AsRef<str>>(dmi: S) -> Vec<Self> {
        trace!("Parsing udevadm output…");
        let dmi = dmi.as_ref();

        let devices_amount: usize = RE_NUM_MEMORY_DEVICES
            .captures(dmi)
            .and_then(|captures| captures.get(1))
            .and_then(|capture| capture.as_str().parse().ok())
            .unwrap_or(0);

        let mut devices = Vec::with_capacity(devices_amount);

        for i in 0..devices_amount {
            let i = i.to_string();

            let installed = Regex::new(&TEMPLATE_RE_PRESENT.replace('%', &i))
                .ok()
                .and_then(|regex| regex.captures(dmi))
                .and_then(|captures| captures.get(1))
                .and_then(|capture| capture.as_str().parse::<usize>().ok())
                != Some(0);

            let speed = if installed {
                Regex::new(&TEMPLATE_RE_CONFIGURED_SPEED_MTS.replace('%', &i))
                    .ok()
                    .and_then(|regex| regex.captures(dmi))
                    .or_else(|| {
                        Regex::new(&TEMPLATE_RE_SPEED_MTS.replace('%', &i.to_string()))
                            .ok()
                            .and_then(|regex| regex.captures(dmi))
                    })
                    .and_then(|captures| captures.get(1))
                    .and_then(|capture| capture.as_str().parse().ok())
            } else {
                None
            };

            let form_factor = Regex::new(&TEMPLATE_RE_FORM_FACTOR.replace('%', &i))
                .ok()
                .and_then(|regex| regex.captures(dmi))
                .and_then(|captures| captures.get(1))
                .map(|capture| capture.as_str().to_string());

            let r#type = Regex::new(&TEMPLATE_RE_TYPE.replace('%', &i))
                .ok()
                .and_then(|regex| regex.captures(dmi))
                .and_then(|captures| captures.get(1))
                .map(|capture| capture.as_str().to_string())
                .filter(|capture| capture != "<OUT OF SPEC>");

            let type_detail = Regex::new(&TEMPLATE_RE_TYPE_DETAIL.replace('%', &i))
                .ok()
                .and_then(|regex| regex.captures(dmi))
                .and_then(|captures| captures.get(1))
                .map(|capture| capture.as_str().to_string());

            let size = Regex::new(&TEMPLATE_RE_SIZE.replace('%', &i))
                .ok()
                .and_then(|regex| regex.captures(dmi))
                .and_then(|captures| captures.get(1))
                .and_then(|capture| capture.as_str().parse().ok());

            let memory_device = Self {
                speed_mts: speed,
                form_factor,
                r#type,
                type_detail,
                size,
                installed,
            };

            trace!("Found memory device: {memory_device:?}");

            devices.push(memory_device);
        }

        devices
    }

    pub fn get() -> Result<Vec<Self>> {
        let virtual_dmi = Self::virtual_dmi();
        if virtual_dmi.is_empty() {
            let output = Command::new("dmidecode")
                .args(["-t", "17", "-q"])
                .output()?;
            if output.status.code().unwrap_or(1) == 1 {
                debug!("Unable to get memory information without elevated privileges");
                bail!("no permission")
            }
            debug!("Memory information obtained using dmidecode (unprivileged)");
            Ok(Self::parse_dmidecode(String::from_utf8(output.stdout)?))
        } else {
            debug!("Memory information obtained using udevadm");
            Ok(virtual_dmi)
        }
    }

    pub fn pkexec_dmidecode() -> Result<Vec<Self>> {
        debug!("Using pkexec to get memory information (dmidecode)…");
        let output = if *IS_FLATPAK {
            Command::new(FLATPAK_SPAWN)
                .args([
                    "--host",
                    "/usr/bin/pkexec",
                    "--disable-internal-agent",
                    &format!("{}/bin/dmidecode", FLATPAK_APP_PATH.as_str()),
                    "-t",
                    "17",
                    "-q",
                ])
                .output()?
        } else {
            Command::new("pkexec")
                .args(["--disable-internal-agent", "dmidecode", "-t", "17", "-q"])
                .output()?
        };
        debug!("Memory information obtained using dmidecode (privileged)");
        Ok(Self::parse_dmidecode(
            String::from_utf8(output.stdout)?.as_str(),
        ))
    }
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use crate::devices::memory::MemoryDevice;

    const DMIDECODE_OUTPUT: &str = concat!(
        "Memory Device\n",
        "        Total Width: Unknown\n",
        "        Data Width: Unknown\n",
        "        Size: No Module Installed\n",
        "        Form Factor: Unknown\n",
        "        Set: None\n",
        "        Locator: DIMM 0\n",
        "        Bank Locator: P0 CHANNEL A\n",
        "        Type: Unknown\n",
        "        Type Detail: Unknown\n",
        "\n",
        "Memory Device\n",
        "        Total Width: 64 bits\n",
        "        Data Width: 64 bits\n",
        "        Size: 16 GB\n",
        "        Form Factor: DIMM\n",
        "        Set: None\n",
        "        Locator: DIMM 1\n",
        "        Bank Locator: P0 CHANNEL A\n",
        "        Type: DDR4\n",
        "        Type Detail: Synchronous Unbuffered (Unregistered)\n",
        "        Speed: 3000 MT/s\n",
        "        Manufacturer: Unknown\n",
        "        Serial Number: 00000000\n",
        "        Asset Tag: Not Specified\n",
        "        Part Number: 123\n",
        "        Rank: 1\n",
        "        Configured Memory Speed: 3000 MT/s\n",
        "        Minimum Voltage: 1.2 V\n",
        "        Maximum Voltage: 1.2 V\n",
        "        Configured Voltage: 1.2 V"
    );

    const UDEVADM_OUTPUT: &str = concat!(
        "E: MEMORY_ARRAY_LOCATION=System Board Or Motherboard\n",
        "E: MEMORY_ARRAY_MAX_CAPACITY=137438953472\n",
        "E: MEMORY_DEVICE_0_PRESENT=0\n",
        "E: MEMORY_DEVICE_0_FORM_FACTOR=Unknown\n",
        "E: MEMORY_DEVICE_0_LOCATOR=DIMM 0\n",
        "E: MEMORY_DEVICE_0_BANK_LOCATOR=P0 CHANNEL A\n",
        "E: MEMORY_DEVICE_0_TYPE=Unknown\n",
        "E: MEMORY_DEVICE_0_TYPE_DETAIL=Unknown\n",
        "E: MEMORY_DEVICE_0_SPEED_MTS=3000\n",
        "E: MEMORY_DEVICE_0_MANUFACTURER=Unknown\n",
        "E: MEMORY_DEVICE_0_SERIAL_NUMBER=Unknown\n",
        "E: MEMORY_DEVICE_0_ASSET_TAG=Not Specified\n",
        "E: MEMORY_DEVICE_0_PART_NUMBER=Unknown\n",
        "E: MEMORY_DEVICE_0_CONFIGURED_SPEED_MTS=3000\n",
        "E: MEMORY_DEVICE_1_TOTAL_WIDTH=64\n",
        "E: MEMORY_DEVICE_1_DATA_WIDTH=64\n",
        "E: MEMORY_DEVICE_1_SIZE=17179869184\n",
        "E: MEMORY_DEVICE_1_FORM_FACTOR=DIMM\n",
        "E: MEMORY_DEVICE_1_LOCATOR=DIMM 1\n",
        "E: MEMORY_DEVICE_1_BANK_LOCATOR=P0 CHANNEL A\n",
        "E: MEMORY_DEVICE_1_TYPE=DDR4\n",
        "E: MEMORY_DEVICE_1_TYPE_DETAIL=Synchronous Unbuffered (Unregistered)\n",
        "E: MEMORY_DEVICE_1_SPEED_MTS=3000\n",
        "E: MEMORY_DEVICE_1_MANUFACTURER=Unknown\n",
        "E: MEMORY_DEVICE_1_SERIAL_NUMBER=00000000\n",
        "E: MEMORY_DEVICE_1_ASSET_TAG=Not Specified\n",
        "E: MEMORY_DEVICE_1_PART_NUMBER=123\n",
        "E: MEMORY_DEVICE_1_RANK=1\n",
        "E: MEMORY_DEVICE_1_CONFIGURED_SPEED_MTS=3000\n",
        "E: MEMORY_DEVICE_1_MINIMUM_VOLTAGE=1\n",
        "E: MEMORY_DEVICE_1_MAXIMUM_VOLTAGE=1\n",
        "E: MEMORY_DEVICE_1_CONFIGURED_VOLTAGE=1\n",
        "E: MEMORY_DEVICE_2_PRESENT=0\n",
        "E: MEMORY_ARRAY_NUM_DEVICES=2"
    );

    #[test]
    fn valid_dmidecode_complex() {
        let parsed = MemoryDevice::parse_dmidecode(DMIDECODE_OUTPUT);

        let expected = vec![
            MemoryDevice {
                speed_mts: None,
                form_factor: Some("Unknown".into()),
                r#type: Some("Unknown".into()),
                type_detail: Some("Unknown".into()),
                size: None,
                installed: false,
            },
            MemoryDevice {
                speed_mts: Some(3000),
                form_factor: Some("DIMM".into()),
                r#type: Some("DDR4".into()),
                type_detail: Some("Synchronous Unbuffered (Unregistered)".into()),
                size: Some(17179869184),
                installed: true,
            },
        ];

        assert_eq!(expected, parsed);
    }

    #[test]
    fn valid_udevadm_complex() {
        let parsed = MemoryDevice::parse_virtual_dmi(UDEVADM_OUTPUT);

        let expected = vec![
            MemoryDevice {
                speed_mts: None,
                form_factor: Some("Unknown".into()),
                r#type: Some("Unknown".into()),
                type_detail: Some("Unknown".into()),
                size: None,
                installed: false,
            },
            MemoryDevice {
                speed_mts: Some(3000),
                form_factor: Some("DIMM".into()),
                r#type: Some("DDR4".into()),
                type_detail: Some("Synchronous Unbuffered (Unregistered)".into()),
                size: Some(17179869184),
                installed: true,
            },
        ];

        assert_eq!(expected, parsed);
    }

    #[test]
    fn udevadm_dmidecode_equal() {
        let dmidecode = MemoryDevice::parse_dmidecode(DMIDECODE_OUTPUT);
        let udevadm = MemoryDevice::parse_virtual_dmi(UDEVADM_OUTPUT);

        assert_eq!(dmidecode, udevadm);
    }
}
