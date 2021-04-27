#![no_std]
#![allow(incomplete_features)]
#![feature(min_type_alias_impl_trait)]
#![feature(generic_associated_types)]

pub(crate) mod fmt;

mod buffer;
mod num;
mod parser;
mod protocol;
mod socket_pool;

use crate::fmt::*;
use socket_pool::SocketPool;

use buffer::Buffer;
use core::{
    cell::{RefCell, UnsafeCell},
    future::Future,
    pin::Pin,
};
use drogue_device_kernel::channel::*;
use drogue_network::{
    ip::{IpAddress, IpProtocol, SocketAddress},
    tcp::{TcpError, TcpStack},
    wifi::{Join, JoinError, WifiSupplicant},
};
use embassy::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use embedded_hal::digital::v2::OutputPin;
use futures::future::{select, Either};
use futures::pin_mut;
use heapless::{String, Vec};
use protocol::{Command, ConnectionType, Response as AtResponse, WiFiMode};

pub const BUFFER_LEN: usize = 512;

#[derive(Debug)]
pub enum AdapterError {
    UnableToInitialize,
    NoAvailableSockets,
    Timeout,
    UnableToOpen,
    UnableToClose,
    WriteError,
    ReadError,
    InvalidSocket,
    OperationNotSupported,
}

pub struct Esp8266Controller<'a> {
    socket_pool: SocketPool,
    command_producer: ChannelSender<'a, String<consts::U128>, consts::U2>,
    response_consumer: ChannelReceiver<'a, AtResponse, consts::U2>,
    notification_consumer: ChannelReceiver<'a, AtResponse, consts::U2>,
}

pub struct Esp8266Modem<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    uart: UART,
    enable: ENABLE,
    reset: RESET,
    parse_buffer: Buffer,
    command_consumer: ChannelReceiver<'a, String<consts::U128>, consts::U2>,
    response_producer: ChannelSender<'a, AtResponse, consts::U2>,
    notification_producer: ChannelSender<'a, AtResponse, consts::U2>,
}

pub struct Esp8266Driver<UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    enable: Option<ENABLE>,
    reset: Option<RESET>,
    uart: Option<UART>,
    command_channel: Channel<String<consts::U128>, consts::U2>,
    response_channel: Channel<AtResponse, consts::U2>,
    notification_channel: Channel<AtResponse, consts::U2>,
}

impl<UART, ENABLE, RESET> Esp8266Driver<UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    pub fn new(uart: UART, enable: ENABLE, reset: RESET) -> Self {
        Self {
            uart: Some(uart),
            command_channel: Channel::new(),
            response_channel: Channel::new(),
            notification_channel: Channel::new(),
            enable: Some(enable),
            reset: Some(reset),
        }
    }

    pub fn initialize<'a>(
        &'a mut self,
    ) -> (Esp8266Controller<'a>, Esp8266Modem<'a, UART, ENABLE, RESET>) {
        let (cp, cc) = self.command_channel.split();
        let (rp, rc) = self.response_channel.split();
        let (np, nc) = self.notification_channel.split();

        let mut modem = Esp8266Modem::new(
            self.uart.take().unwrap(),
            self.enable.take().unwrap(),
            self.reset.take().unwrap(),
            cc,
            rp,
            np,
        );
        let controller = Esp8266Controller::new(cp, rc, nc);

        (controller, modem)
    }

    /*


    // Await input from uart and attempt to digest input
    pub async fn run(&'a mut self) -> Result<(), AdapterError> {
        loop {
            let mut buf = [0; 1];
            let command_fut = self.command_consumer.receive();
            let uart_fut = self.uart.read(&mut buf[..]);

            match select(command_fut, uart_fut).await {
                Either::Left((r, _)) => {
                    // Write command to uart
                }
                Either::Right(_) => {
                    self.parse_buffer.write(buf[0]).unwrap();
                    self.digest().await
                }
            }
        }
    }

    /*
    async fn start(mut self) -> Self {
        info!("Starting ESP8266 Modem");
        loop {
            if let Err(e) = self.process().await {
                error!("Error reading data: {:?}", e);
            }

            if let Err(e) = self.digest().await {
                error!("Error digesting data");
            }
        }
    }
    */






    async fn send<'c>(&mut self, command: Command<'c>) -> Result<AtResponse, AdapterError> {
        let bytes = command.as_bytes();
        trace!(
            "writing command {}",
            core::str::from_utf8(bytes.as_bytes()).unwrap()
        );

        self.uart
            .write(&bytes.as_bytes())
            .await
            .map_err(|e| AdapterError::WriteError)?;

        self.uart
            .write(b"\r\n")
            .await
            .map_err(|e| AdapterError::WriteError)?;

        Ok(self.wait_for_response().await)
    }

    async fn wait_for_response(&mut self) -> AtResponse {
        self.response_queue.receive().await
    }

    async fn set_wifi_mode(&mut self, mode: WiFiMode) -> Result<(), ()> {
        let command = Command::SetMode(mode);
        match self.send(command).await {
            Ok(AtResponse::Ok) => Ok(()),
            _ => Err(()),
        }
    }

    async fn join_wep(&mut self, ssid: &str, password: &str) -> Result<IpAddress, JoinError> {
        let command = Command::JoinAp { ssid, password };
        match self.send(command).await {
            Ok(AtResponse::Ok) => self.get_ip_address().await.map_err(|_| JoinError::Unknown),
            Ok(AtResponse::WifiConnectionFailure(reason)) => {
                log::warn!("Error connecting to wifi: {:?}", reason);
                Err(JoinError::Unknown)
            }
            _ => Err(JoinError::UnableToAssociate),
        }
    }

    async fn get_ip_address(&mut self) -> Result<IpAddress, ()> {
        let command = Command::QueryIpAddress;

        if let Ok(AtResponse::IpAddresses(addresses)) = self.send(command).await {
            return Ok(IpAddress::V4(addresses.ip));
        }

        Err(())
    }

    async fn process_notifications(&mut self) {
        while let Some(response) = self.notification_queue.try_receive().await {
            match response {
                AtResponse::DataAvailable { link_id, len } => {
                    //  shared.socket_pool // [link_id].available += len;
                }
                AtResponse::Connect(_) => {}
                AtResponse::Closed(link_id) => {
                    self.socket_pool.close(link_id as u8);
                }
                _ => { /* ignore */ }
            }
        }
    }
    */
}
/*

impl<'a, UART, ENABLE, RESET> WifiSupplicant for Esp8266Wifi<'a, UART, ENABLE, RESET>
where
    UART: Read + Write + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    type JoinFuture<'m> = impl Future<Output = Result<IpAddress, JoinError>> + 'm;
    fn join<'m>(&'m mut self, join_info: Join) -> Self::JoinFuture<'m> {
        async move {
            match join_info {
                Join::Open => Err(JoinError::Unknown),
                Join::Wpa { ssid, password } => {
                    self.join_wep(ssid.as_ref(), password.as_ref()).await
                }
            }
        }
    }
}

*/

impl<'a, UART, ENABLE, RESET> Esp8266Modem<'a, UART, ENABLE, RESET>
where
    UART: AsyncBufRead + AsyncBufReadExt + AsyncWrite + AsyncWriteExt + 'static,
    ENABLE: OutputPin + 'static,
    RESET: OutputPin + 'static,
{
    pub fn new(
        uart: UART,
        enable: ENABLE,
        reset: RESET,
        command_consumer: ChannelReceiver<'a, String<consts::U128>, consts::U2>,
        response_producer: ChannelSender<'a, AtResponse, consts::U2>,
        notification_producer: ChannelSender<'a, AtResponse, consts::U2>,
    ) -> Self {
        Self {
            uart,
            enable,
            reset,
            parse_buffer: Buffer::new(),
            command_consumer,
            response_producer,
            notification_producer,
        }
    }

    async fn initialize(&mut self) {
        let mut buffer: [u8; 1024] = [0; 1024];
        let mut pos = 0;

        const READY: [u8; 7] = *b"ready\r\n";

        info!("waiting for adapter to become ready");

        self.enable.set_high().ok().unwrap();
        self.reset.set_high().ok().unwrap();

        let mut rx_buf = [0; 1];
        loop {
            let result = uart_read(&mut self.uart, &mut rx_buf[..]).await;
            match result {
                Ok(c) => {
                    buffer[pos] = rx_buf[0];
                    pos += 1;
                    if pos >= READY.len() && buffer[pos - READY.len()..pos] == READY {
                        info!("adapter is ready");
                        self.disable_echo()
                            .await
                            .expect("Error disabling echo mode");
                        info!("Echo disabled");
                        self.enable_mux().await.expect("Error enabling mux");
                        info!("Mux enabled");
                        self.set_recv_mode()
                            .await
                            .expect("Error setting receive mode");
                        info!("Recv mode configured");
                        self.set_mode().await.expect("Error setting station mode");
                        info!("adapter configured");
                        break;
                    }
                }
                Err(e) => {
                    error!("Error initializing ESP8266 modem");
                    break;
                }
            }
        }
    }

    async fn disable_echo(&mut self) -> Result<(), AdapterError> {
        info!("Disabling echo");
        uart_write(&mut self.uart, b"ATE0\r\n")
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?;
        info!("Command sent");
        Ok(self
            .wait_for_ok()
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?)
    }

    async fn enable_mux(&mut self) -> Result<(), AdapterError> {
        uart_write(&mut self.uart, b"AT+CIPMUX=1\r\n")
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?;
        Ok(self
            .wait_for_ok()
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?)
    }

    async fn set_recv_mode(&mut self) -> Result<(), AdapterError> {
        uart_write(&mut self.uart, b"AT+CIPRECVMODE=1\r\n")
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?;
        Ok(self
            .wait_for_ok()
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?)
    }

    async fn set_mode(&mut self) -> Result<(), AdapterError> {
        uart_write(&mut self.uart, b"AT+CWMODE_CUR=1\r\n")
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?;
        Ok(self
            .wait_for_ok()
            .await
            .map_err(|_| AdapterError::UnableToInitialize)?)
    }

    async fn wait_for_ok(&mut self) -> Result<(), AdapterError> {
        let mut buf: [u8; 64] = [0; 64];
        let mut pos = 0;

        loop {
            uart_read(&mut self.uart, &mut buf[pos..pos + 1])
                .await
                .map_err(|_| AdapterError::ReadError)?;
            pos += 1;
            if buf[0..pos].ends_with(b"OK\r\n") {
                return Ok(());
            } else if buf[0..pos].ends_with(b"ERROR\r\n") {
                return Err(AdapterError::UnableToInitialize);
            }
        }
    }

    /// Run the processing loop until an error is encountered
    pub async fn run(&mut self) -> ! {
        // Result<(), AdapterError> where Self: 'a {
        self.initialize().await;
        loop {
            let mut buf = [0; 1];
            let (cmd, input) = {
                let command_fut = self.command_consumer.receive();
                let uart_fut = uart_read(&mut self.uart, &mut buf[..]);
                pin_mut!(uart_fut);

                match select(command_fut, uart_fut).await {
                    Either::Left((s, _)) => (Some(s), None),
                    Either::Right((r, _)) => (None, Some(r)),
                }
            };
            // We got command to write, write it
            if let Some(s) = cmd {
                uart_write(&mut self.uart, s.as_bytes()).await;
            }

            // We got input, digest it
            if let Some(input) = input {
                match input {
                    Ok(len) => {
                        for b in &buf[..len] {
                            self.parse_buffer.write(*b).unwrap();
                        }
                        self.digest().await;
                    }
                    Err(e) => {
                        error!("Error reading from uart: {:?}", e);
                    }
                }
            }
        }
    }

    async fn digest(&mut self) -> Result<(), AdapterError> {
        let result = self.parse_buffer.parse();

        if let Ok(response) = result {
            if !matches!(response, AtResponse::None) {
                //trace!("--> {:?}", response);
            }
            match response {
                AtResponse::None => {}
                AtResponse::Ok
                | AtResponse::Error
                | AtResponse::FirmwareInfo(..)
                | AtResponse::Connect(..)
                | AtResponse::ReadyForData
                | AtResponse::ReceivedDataToSend(..)
                | AtResponse::DataReceived(..)
                | AtResponse::SendOk
                | AtResponse::SendFail
                | AtResponse::WifiConnectionFailure(..)
                | AtResponse::IpAddress(..)
                | AtResponse::Resolvers(..)
                | AtResponse::DnsFail
                | AtResponse::UnlinkFail
                | AtResponse::IpAddresses(..) => {
                    self.response_producer.send(response).await;
                }
                AtResponse::Closed(..) | AtResponse::DataAvailable { .. } => {
                    self.notification_producer.send(response).await;
                }
                AtResponse::WifiConnected => {
                    info!("wifi connected");
                }
                AtResponse::WifiDisconnect => {
                    info!("wifi disconnect");
                }
                AtResponse::GotIp => {
                    info!("wifi got ip");
                }
            }
        }
        Ok(())
    }
}

impl<'a> Esp8266Controller<'a> {
    pub fn new(
        command_producer: ChannelSender<'a, String<consts::U128>, consts::U2>,
        response_consumer: ChannelReceiver<'a, AtResponse, consts::U2>,
        notification_consumer: ChannelReceiver<'a, AtResponse, consts::U2>,
    ) -> Self {
        Self {
            socket_pool: SocketPool::new(),
            command_producer,
            response_consumer,
            notification_consumer,
        }
    }

    async fn send<'c>(&self, command: Command<'c>) -> Result<AtResponse, AdapterError> {
        let mut bytes = command.as_bytes();
        trace!(
            "writing command {}",
            core::str::from_utf8(bytes.as_bytes()).unwrap()
        );

        bytes.push_str("\r\n");
        self.command_producer.send(bytes).await;
        Ok(self.response_consumer.receive().await)
    }

    async fn set_wifi_mode(&self, mode: WiFiMode) -> Result<(), ()> {
        let command = Command::SetMode(mode);
        match self.send(command).await {
            Ok(AtResponse::Ok) => Ok(()),
            _ => Err(()),
        }
    }

    async fn join_wep(&self, ssid: &str, password: &str) -> Result<IpAddress, JoinError> {
        let command = Command::JoinAp { ssid, password };
        match self.send(command).await {
            Ok(AtResponse::Ok) => self.get_ip_address().await.map_err(|_| JoinError::Unknown),
            Ok(AtResponse::WifiConnectionFailure(reason)) => {
                log::warn!("Error connecting to wifi: {:?}", reason);
                Err(JoinError::Unknown)
            }
            _ => Err(JoinError::UnableToAssociate),
        }
    }

    async fn get_ip_address(&self) -> Result<IpAddress, ()> {
        let command = Command::QueryIpAddress;

        if let Ok(AtResponse::IpAddresses(addresses)) = self.send(command).await {
            return Ok(IpAddress::V4(addresses.ip));
        }

        Err(())
    }

    async fn process_notifications(&mut self) {
        while let Some(response) = self.notification_consumer.try_receive().await {
            match response {
                AtResponse::DataAvailable { link_id, len } => {
                    //  shared.socket_pool // [link_id].available += len;
                }
                AtResponse::Connect(_) => {}
                AtResponse::Closed(link_id) => {
                    self.socket_pool.close(link_id as u8);
                }
                _ => { /* ignore */ }
            }
        }
    }
}

impl<'a> WifiSupplicant for Esp8266Controller<'a> {
    #[rustfmt::skip]
    type JoinFuture<'m> where 'a: 'm = impl Future<Output = Result<IpAddress, JoinError>> + 'm;
    fn join<'m>(&'m mut self, join_info: Join) -> Self::JoinFuture<'m> {
        async move {
            match join_info {
                Join::Open => Err(JoinError::Unknown),
                Join::Wpa { ssid, password } => {
                    self.join_wep(ssid.as_ref(), password.as_ref()).await
                }
            }
        }
    }
}

impl<'a> TcpStack for Esp8266Controller<'a> {
    type SocketHandle = u8;

    #[rustfmt::skip]
    type OpenFuture<'m> where 'a: 'm = impl Future<Output = Self::SocketHandle> + 'm;
    fn open<'m>(&'m mut self) -> Self::OpenFuture<'m> {
        async move { self.socket_pool.open().await }
    }

    #[rustfmt::skip]
    type ConnectFuture<'m> where 'a: 'm = impl Future<Output = Result<(), TcpError>> + 'm;
    fn connect<'m>(
        &'m mut self,
        handle: Self::SocketHandle,
        proto: IpProtocol,
        dst: SocketAddress,
    ) -> Self::ConnectFuture<'m> {
        async move {
            let command = Command::StartConnection(handle as usize, ConnectionType::TCP, dst);
            if let Ok(AtResponse::Connect(..)) = self.send(command).await {
                Ok(())
            } else {
                Err(TcpError::ConnectError)
            }
        }
    }

    #[rustfmt::skip]
    type WriteFuture<'m> where 'a: 'm = impl Future<Output = Result<usize, TcpError>> + 'm;
    fn write<'m>(&'m mut self, handle: Self::SocketHandle, buf: &'m [u8]) -> Self::WriteFuture<'m> {
        async move {
            self.process_notifications().await;
            if self.socket_pool.is_closed(handle) {
                return Err(TcpError::SocketClosed);
            }
            let command = Command::Send {
                link_id: handle as usize,
                len: buf.len(),
            };

            let result = match self.send(command).await {
                Ok(AtResponse::Ok) => {
                    match self.response_consumer.receive().await {
                        AtResponse::ReadyForData => {
                            self.command_producer
                                .send(String::from_utf8(Vec::from_slice(buf).unwrap()).unwrap())
                                .await;
                            let mut data_sent: Option<usize> = None;
                            loop {
                                match self.response_consumer.receive().await {
                                    AtResponse::ReceivedDataToSend(len) => {
                                        data_sent.replace(len);
                                    }
                                    AtResponse::SendOk => break Ok(data_sent.unwrap_or_default()),
                                    _ => {
                                        break Err(TcpError::WriteError);
                                        // unknown response
                                    }
                                }
                            }
                        }
                        r => {
                            info!("Unexpected response: {:?}", r);
                            Err(TcpError::WriteError)
                        }
                    }
                }
                Ok(r) => {
                    info!("Unexpected response: {:?}", r);
                    Err(TcpError::WriteError)
                }
                Err(_) => Err(TcpError::WriteError),
            };
            result
        }
    }

    #[rustfmt::skip]
    type ReadFuture<'m> where 'a: 'm = impl Future<Output = Result<usize, TcpError>> + 'm;
    fn read<'m>(
        &'m mut self,
        handle: Self::SocketHandle,
        buf: &'m mut [u8],
    ) -> Self::ReadFuture<'m> {
        async move {
            let mut rp = 0;
            loop {
                let result = async {
                    self.process_notifications().await;
                    if self.socket_pool.is_closed(handle) {
                        return Err(TcpError::SocketClosed);
                    }

                    let command = Command::Receive {
                        link_id: handle as usize,
                        len: core::cmp::min(buf.len() - rp, BUFFER_LEN),
                    };

                    match self.send(command).await {
                        Ok(AtResponse::DataReceived(inbound, len)) => {
                            for (i, b) in inbound[0..len].iter().enumerate() {
                                buf[rp + i] = *b;
                            }
                            Ok(len)
                        }
                        Ok(AtResponse::Ok) => Ok(0),
                        _ => Err(TcpError::ReadError),
                    }
                }
                .await;

                match result {
                    Ok(len) => {
                        rp += len;
                        if len == 0 || rp == buf.len() {
                            return Ok(rp);
                        }
                    }
                    Err(e) => {
                        if rp == 0 {
                            return Err(e);
                        } else {
                            return Ok(rp);
                        }
                    }
                }
            }
        }
    }

    #[rustfmt::skip]
    type CloseFuture<'m> where 'a: 'm = impl Future<Output = ()> + 'm;
    fn close<'m>(&'m mut self, handle: Self::SocketHandle) -> Self::CloseFuture<'m> {
        async move {
            let command = Command::CloseConnection(handle as usize);
            match self.send(command).await {
                Ok(AtResponse::Ok) | Ok(AtResponse::UnlinkFail) => {
                    self.socket_pool.close(handle);
                }
                _ => {}
            }
        }
    }
}

async fn uart_read<UART>(uart: &mut UART, rx_buf: &mut [u8]) -> Result<usize, embassy::io::Error>
where
    UART: AsyncBufRead + AsyncBufReadExt + 'static,
{
    let mut uart = unsafe { Pin::new_unchecked(uart) };
    uart.read(rx_buf).await
}

async fn uart_write<UART>(uart: &mut UART, buf: &[u8]) -> Result<(), embassy::io::Error>
where
    UART: AsyncWriteExt + AsyncWrite + 'static,
{
    let mut uart = unsafe { Pin::new_unchecked(uart) };
    uart.write_all(buf).await
}
