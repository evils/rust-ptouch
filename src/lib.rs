use std::time::Duration;

use commands::Commands;
use device::Status;
use log::{debug, error, trace, warn};
use structopt::StructOpt;

use rusb::{Context, Device, DeviceDescriptor, DeviceHandle, Direction, TransferType, UsbContext};

pub mod device;
use device::*;

pub mod commands;

pub mod tiff;

pub mod render;

pub struct PTouch {
    _device: Device<Context>,
    handle: DeviceHandle<Context>,
    descriptor: DeviceDescriptor,
    //endpoints: Endpoints,
    timeout: Duration,

    cmd_ep: u8,
    stat_ep: u8,
}

pub const BROTHER_VID: u16 = 0x04F9;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);

/// Filter for selecting a specific PTouch device
#[derive(Clone, PartialEq, Debug)]
#[cfg_attr(feature = "structopt", derive(StructOpt))]
pub struct Filter {
    #[cfg_attr(feature = "structopt", structopt(long, default_value = "pt-p710bt"))]
    /// Label maker device kind
    pub device: device::PTouchDevice,

    #[cfg_attr(feature = "structopt", structopt(long, default_value = "0"))]
    /// Index (if multiple devices are connected)
    pub index: usize,
}

// Lazy initialised libusb context
lazy_static::lazy_static! {
    static ref CONTEXT: Context = {
        Context::new().unwrap()
    };
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("USB error: {:?}", 0)]
    Usb(rusb::Error),

    #[error("Invalid device index")]
    InvalidIndex,

    #[error("No supported languages")]
    NoLanguages,

    #[error("Unable to locate expected endpoints")]
    InvalidEndpoints,

    #[error("Renderer error")]
    Render,

    #[error("Operation timeout")]
    Timeout,

    #[error("PTouch Error ({:?} {:?})", 0, 1)]
    PTouch(Error1, Error2),
}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        Error::Usb(e)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

impl PTouch {
    /// Create a new PTouch driver with the provided USB options
    pub fn new(o: &Filter) -> Result<Self, Error> {
        Self::new_with_context(o, &CONTEXT)
    }

    /// Create a new PTouch driver with the provided USB options and rusb::Context
    pub fn new_with_context(o: &Filter, context: &Context) -> Result<Self, Error> {
        // List available devices
        let devices = context.devices()?;

        // Find matching VID/PIDs
        let mut matches: Vec<_> = devices
            .iter()
            .filter_map(|d| {
                // Fetch device descriptor
                let desc = match d.device_descriptor() {
                    Ok(d) => d,
                    Err(e) => {
                        debug!("Could not fetch descriptor for device {:?}: {:?}", d, e);
                        return None;
                    }
                };

                // Return devices matching vid/pid filters
                if desc.vendor_id() == BROTHER_VID && desc.product_id() == o.device as u16 {
                    Some((d, desc))
                } else {
                    None
                }
            })
            .collect();

        // Check index is valid
        if matches.len() < o.index || matches.len() == 0 {
            error!(
                "Device index ({}) exceeds number of discovered devices ({})",
                o.index,
                matches.len()
            );
            return Err(Error::InvalidIndex);
        }

        debug!("Found matching devices: {:?}", matches);

        // Fetch matching device
        let (device, descriptor) = matches.remove(o.index);

        // Open device handle
        let mut handle = match device.open() {
            Ok(v) => v,
            Err(e) => {
                error!("Error opening device");
                return Err(e.into());
            }
        };

        // Reset device
        if let Err(e) = handle.reset() {
            error!("Error resetting device handle");
            return Err(e.into())
        }

        // Locate endpoints
        let config_desc = match device.config_descriptor(0) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to fetch config descriptor");
                return Err(e.into());
            }
        };

        let interface = match config_desc.interfaces().next() {
            Some(i) => i,
            None => {
                error!("No interfaces found");
                return Err(Error::InvalidEndpoints);
            }
        };

        // EP1 is a bulk IN (printer -> PC) endpoint for status messages
        // EP2 is a bulk OUT (PC -> printer) endpoint for print commands
        // TODO: is this worth it, could we just, hard-code the endpoints?
        let (mut cmd_ep, mut stat_ep) = (None, None);

        for interface_desc in interface.descriptors() {
            for endpoint_desc in interface_desc.endpoint_descriptors() {
                // Find the relevant endpoints
                match (endpoint_desc.transfer_type(), endpoint_desc.direction()) {
                    (TransferType::Bulk, Direction::In) => stat_ep = Some(endpoint_desc.address()),
                    (TransferType::Bulk, Direction::Out) => cmd_ep = Some(endpoint_desc.address()),
                    (_, _) => continue,
                }
            }
        }

        let (cmd_ep, stat_ep) = match (cmd_ep, stat_ep) {
            (Some(cmd), Some(stat)) => (cmd, stat),
            _ => {
                error!("Failed to locate command and status endpoints");
                return Err(Error::InvalidEndpoints);
            }
        };

        // Detach kernel driver
        debug!("Checking for active kernel driver");
        match handle.kernel_driver_active(interface.number())? {
            true => {
                debug!("Detaching kernel driver");
                handle.detach_kernel_driver(interface.number())?;
            },
            false => {
                debug!("Kernel driver inactive");
            },
        }

        debug!("Claiming interface");
        handle.claim_interface(interface.number())?;

        // Set endpoint configuration
        #[cfg(off)]
        if let Err(e) = handle.set_active_configuration(config_desc.number()) {
            error!("Failed to set active configuration");
            return Err(e.into());
        }

        let mut s = Self {
            _device: device,
            handle,
            descriptor,
            cmd_ep,
            stat_ep,
            timeout: DEFAULT_TIMEOUT,
        };


        s.invalidate()?;

        s.init()?;

        Ok(s)
    }

    /// Fetch device information
    pub fn info(&mut self) -> Result<Info, Error> {
        let timeout = Duration::from_millis(200);

        // Fetch base configuration
        let languages = self.handle.read_languages(timeout)?;
        let active_config = self.handle.active_configuration()?;

        trace!("Active configuration: {}", active_config);
        trace!("Languages: {:?}", languages);

        // Check a language is available
        if languages.len() == 0 {
            return Err(Error::NoLanguages);
        }

        // Fetch information
        let language = languages[0];
        let manufacturer =
            self.handle
                .read_manufacturer_string(language, &self.descriptor, timeout)?;
        let product = self
            .handle
            .read_product_string(language, &self.descriptor, timeout)?;
        let serial = self
            .handle
            .read_serial_number_string(language, &self.descriptor, timeout)?;

        Ok(Info {
            manufacturer,
            product,
            serial,
        })
    }

    pub fn status(&mut self) -> Result<Status, Error> {
        // Issue status request
        self.status_req()?;

        // Read status response
        let d = self.read(self.timeout)?;

        // Convert to status object
        let s = Status::from(d);

        debug!("Status: {:02x?}", s);

        Ok(s)
    }

    pub fn print_raw(&mut self, data: Vec<[u8; 16]>, info: &PrintInfo) -> Result<(), Error> {
        // TODO: should we check things are compatible here?


        // Print sequence from raster guide Section 2.1
        // 1. Set to raster mode
        self.switch_mode(Mode::Raster)?;

        // 2. Enable status notification
        self.set_status_notify(true)?;

        // 3. Set print information (media type etc.)
        self.set_print_info(info)?;

        // 4. Set various mode settings
        self.set_various_mode(VariousMode::AUTO_CUT)?;

        // 5. Specify page number in "cut each * labels"
        // Note this is not supported on the PT-P710BT

        // 6. Set advanced mode settings
        self.set_advanced_mode(AdvancedMode::NO_CHAIN)?;

        // 7. Specify margin amount
        // TODO: based on what?
        self.set_margin(14)?;

        // 8. Set compression mode
        self.set_compression_mode(CompressionMode::Tiff)?;

        // Check we're ready to print
        //let s = self.status()?;

        //if !s.error1.is_empty() || !s.error2.is_empty() {
        //    debug!("Print error: {:?} {:?}", s.error1, s.error2);
        //    return Err(Error::PTouch(s.error1, s.error2));
        //}

        #[cfg(nope)]
        for i in 0..3 {
            self.raster_zero()?;
        }

        // Send raster data
        for line in data {
            let l = tiff::compress(&line);

            self.raster_transfer(&l)?;
        }

        #[cfg(nope)]
        for i in 0..3 {
            self.raster_zero()?;
        }

        // TODO: looks like 2 bytes of raster data, then a lot of empty lines, then some uncompressed data and some more empty lines?
        // It doesn't _appear_ that there is a further raster header after the initial one
        // so the raster_number _must_ be used to infer when the buffer is filled?
        // 2 bytes raster, seems invalid, how long could this be? one line?
        // (4 * 16) - 1 = 47 lines clear
        // 16 bytes uncompressed, one line
        // (5 * 16) + 8 + 3 = 75 lines clear
        let _example = [
            0x47, 0x02, 0x00, 0x51, // 
            0x00, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0xf6, 0x00, 0x04, 0x16, 0x99, 0x6c, 0x98, 0x6c, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            0x5a, 0x5a,
        ];

        // Execute print operation
        self.print_and_feed()?;

        let mut i = 0;

        // Poll on print completion
        loop {
            if let Ok(s) = self.read_status(self.timeout) {
                if !s.error1.is_empty() || !s.error2.is_empty() {
                    debug!("Print error: {:?} {:?}", s.error1, s.error2);
                    return Err(Error::PTouch(s.error1, s.error2));
                }
    
                if s.status_type == DeviceStatus::PhaseChange {
                    debug!("Started printing");
                }

                if s.status_type == DeviceStatus::Completed {
                    debug!("Print completed");
                    break;
                }
            }

            if i > 10 {
                debug!("Print timeout");
                return Err(Error::Timeout);
            }

            i += 1;

            std::thread::sleep(Duration::from_secs(1));
        }


        Ok(())
    }

    /// Read from status EP (with specified timeout)
    fn read(&mut self, timeout: Duration) -> Result<[u8; 32], Error> {
        let mut buff = [0u8; 32];

        // Execute read
        let n = self.handle.read_bulk(self.stat_ep, &mut buff, timeout)?;

        if n != 32 {
            return Err(Error::Timeout)
        }

        // TODO: parse out status?

        Ok(buff)
    }

    /// Write to command EP (with specified timeout)
    fn write(&mut self, data: &[u8], timeout: Duration) -> Result<(), Error> {
        warn!("WRITE: {:02x?}", data);

        // Execute write
        let n = self.handle.write_bulk(self.cmd_ep, &data, timeout)?;

        // Check write length for timeouts
        if n != data.len() {
            return Err(Error::Timeout)
        }

        Ok(())
    }
}
