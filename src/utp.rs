use std::ffi::{CStr,c_void};
use std::net::SocketAddr;
use std::slice::from_raw_parts;
use std::sync::{Arc,Weak};
use os_socketaddr::OsSocketAddr;

use tokio::net::UdpSocket;
use tokio::sync::{Mutex,Notify,mpsc};
use tokio::time::{Duration,sleep,timeout};

use libutp_sys::*;

pub enum UtpError {
    SocketDestroyed
}

#[derive(Debug)]
struct Buffer<T> {
    read:  Arc<Mutex<mpsc::Receiver<T>>>,
    write: mpsc::Sender<T>
}

#[derive(Debug)]
pub struct UtpSocket {
    pub remote_address: SocketAddr,
    c_socket: *mut utp_socket,
    to_read:  Buffer<Vec<u8>>,
}

unsafe impl Send for UtpSocket {}
unsafe impl Sync for UtpSocket {}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        unsafe { utp_close(self.c_socket) }
    }
}

#[derive(Debug)]
pub struct UtpCtx {
    pub address: SocketAddr,
    incomming:   Buffer<Arc<UtpSocket>>,
    socket:      UdpSocket,
    c_ctx:       *mut utp_context,
    lock:        Mutex<()>
}

impl Drop for UtpCtx {
    fn drop(&mut self) {
        unsafe { utp_destroy(self.c_ctx) }
    }
}

unsafe impl Send for UtpCtx {}
unsafe impl Sync for UtpCtx {}

pub struct Config {}

impl UtpCtx {
    pub async fn new(address: Option<SocketAddr>,_config: Option<Config>) -> Arc<UtpCtx> {
        let default_address = "[::]:0".parse().unwrap();
        let address         = address.unwrap_or(default_address);

        let (write,read) = mpsc::channel(100);
        let incomming    = Buffer {write, read : Arc::new(Mutex::new(read))};

        let listener = UdpSocket::bind(address).await;
        if let Err(err) = listener {
            panic!("couldn't bind to {:?} because of {:?}",address,err);
        }

        let listener = listener.unwrap();

        if let Err(err) = listener.writable().await {
            panic!("socket {:?} didn't become writable {:?}",address,err);
        }

        let ctx = Arc::new(UtpCtx { incomming,
            address: listener.local_addr().unwrap(),
            socket:  listener,
            c_ctx:   unsafe { utp_init(2) },
            lock:    Mutex::new(())
        });


        unsafe {
            utp_set_callback(ctx.c_ctx,UTP_ON_READ as i32,Some(on_read));
            utp_set_callback(ctx.c_ctx,UTP_SENDTO as i32,Some(on_sendto));
            utp_set_callback(ctx.c_ctx,UTP_ON_CONNECT as i32,Some(on_connect));
            utp_set_callback(ctx.c_ctx,UTP_ON_ACCEPT as i32,Some(on_accept));
            utp_set_callback(ctx.c_ctx,UTP_ON_STATE_CHANGE as i32,Some(on_state_change));
            utp_set_callback(ctx.c_ctx,UTP_ON_ERROR as i32,Some(on_error));
            utp_set_callback(ctx.c_ctx,UTP_LOG as i32,Some(log));
            utp_context_set_option(ctx.c_ctx, UTP_LOG_DEBUG as i32,  3);
            utp_context_set_option(ctx.c_ctx, UTP_LOG_NORMAL as i32, 3);
            utp_context_set_option(ctx.c_ctx, UTP_LOG_MTU as i32,    1);

            let ctx_ptr = Weak::into_raw(Arc::downgrade(&ctx));
            utp_context_set_userdata(ctx.c_ctx, ctx_ptr as *mut c_void);
        }

        let _ = tokio::spawn(utp_listener(Arc::downgrade(&ctx)));
        let _ = tokio::spawn(check_timeouts(Arc::downgrade(&ctx)));

        ctx
    }

    pub async fn accept(&self) -> Arc<UtpSocket> {
        let Some(socket) = self.incomming.read.lock().await.recv().await else {
            panic!("error accepting")
        };
        socket
    }

    pub async fn connect(&self, address: SocketAddr) -> Arc<UtpSocket> {
        let utp_socket = {
            let _          = self.lock.lock().await;
            UtpSocket::new(unsafe { utp_create_socket(self.c_ctx)}, address)
        };

        let on_connect = Arc::new(Notify::new());
        let addr : OsSocketAddr = address.into();

        unsafe {
            utp_set_userdata(utp_socket.c_socket, Arc::as_ptr(&on_connect) as *mut c_void);

            if utp_connect(utp_socket.c_socket, addr.as_ptr(), addr.len()) != 0 {
                panic!("couldn't connect to: {:?}", addr)
            }
        }

        on_connect.notified().await;

        unsafe {
            let socket_ptr = Weak::into_raw(Arc::downgrade(&utp_socket));
            utp_set_userdata(utp_socket.c_socket, socket_ptr as *mut c_void);
        }

        utp_socket
    }
}

async fn check_timeouts(ctx: Weak<UtpCtx>) {
    loop {
        sleep(Duration::from_millis(500)).await;
        let Some(ctx) = Weak::upgrade(&ctx) else { break };
        unsafe { utp_check_timeouts(ctx.c_ctx) }
    }
}

async fn utp_listener(ctx: Weak<UtpCtx>) {
    let hundred_ms = Duration::from_millis(100);
    loop {
        let Some(ctx) = Weak::upgrade(&ctx) else { break };

        let mut buffer = vec![0; 1024];

        match timeout(hundred_ms,ctx.socket.recv_from(&mut buffer)).await {
            Err(_)         => continue,
            Ok(Ok((0, _))) => continue,
            Ok(Err(err)) => {
                println!("problem reading from socket: {:?}",err);
                break;
            },
            Ok(Ok((n_bytes, remote_address))) => {
                let c_addr : OsSocketAddr = remote_address.into();
                unsafe {
                    let _   = ctx.lock.lock().await;
                    let ret = utp_process_udp(
                        ctx.c_ctx,buffer.as_ptr(),n_bytes as u64,c_addr.as_ptr(),c_addr.len()
                    );
                    if ret != 1 {
                        println!("got non utp packet")
                    }
                    utp_issue_deferred_acks(ctx.c_ctx);
                };
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn on_read(args: *mut utp_callback_arguments) -> u64 {
    let socket  = &*(utp_get_userdata((*args).socket) as *const UtpSocket);
    let payload = from_raw_parts((*args).buf, (*args).len as usize);

    if let Err(err) = socket.to_read.write.try_send(payload.to_vec()) {
        println!("error reading: {:?} {:?}",socket,err);
    }
    utp_read_drained((*args).socket);
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_sendto(args: *mut utp_callback_arguments) -> u64 {
    let payload   = from_raw_parts((*args).buf, (*args).len as usize).to_vec();
    let addr      = (*args).args1.address;
    let addr_len  = (*args).args2.address_len;

    let Some(dst) = OsSocketAddr::copy_from_raw(addr, addr_len).into_addr() else {
        println!("error creating socketaddr");
        todo!()
    };

    let ctx = &*(utp_context_get_userdata((*args).context) as *const UtpCtx);

    if let Err(err) = ctx.socket.try_send_to(&payload,dst) {
        println!("error sending : {:?}",err);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_connect(args: *mut utp_callback_arguments) -> u64 {
    let on_connect = &*(utp_get_userdata((*args).socket) as *const Notify);
    on_connect.notify_waiters();
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_accept(args: *mut utp_callback_arguments) -> u64 {
    let ctx       = &*(utp_context_get_userdata((*args).context) as *const UtpCtx);
    let addr      = (*args).args1.address;
    let addr_len  = (*args).args2.address_len;

    let Some(remote) = OsSocketAddr::copy_from_raw(addr, addr_len).into_addr() else {
        println!("error creating socketaddr");
        todo!()
    };

    let utp_socket = UtpSocket::new((*args).socket,remote.into());

    if let Err(err) = ctx.incomming.write.try_send(utp_socket) {
        println!("error accpeting: {:?}",err);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_state_change(args: *mut utp_callback_arguments) -> u64 {
    match (*args).args1.state {
        1 => println!("on state change: connect"),
        2 => println!("on state change: writable"),
        3 => println!("on state change: eof"),
        4 => println!("on state change: destroyed"),
        x => println!("on state change: {x}")
    };
    0
}

#[no_mangle]
pub unsafe extern "C" fn log(args: *mut utp_callback_arguments) -> u64 {
    let str = CStr::from_ptr((*args).buf as *const i8);
    println!("{:?}",&str);
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_error(args: *mut utp_callback_arguments) -> u64 {
    match (*args).args1.error_code {
        0 => println!("on error: UTP_ECONNREFUSED"),
        1 => println!("on error: UTP_ECONNRESET"),
        2 => println!("on error: UTP_ETIMEDOUT"),
        x => println!("on error: {x}")
    };
    0
}

impl UtpSocket {
    pub fn new(c_socket: *mut utp_socket, remote_address: SocketAddr) -> Arc<UtpSocket> {
        let (write,read) = mpsc::channel(100);
        let to_read      = Buffer {write, read : Arc::new(Mutex::new(read))};
        let lock         = Mutex::new(());
        let socket       = Arc::new(UtpSocket{remote_address,c_socket,to_read});

        unsafe {
            let socket_ptr = Weak::into_raw(Arc::downgrade(&socket));
            utp_set_userdata(socket.c_socket, socket_ptr as *mut c_void);
        }

        socket
    }

    pub async fn write(&self, payload: &[u8]) -> usize {
        let payload_ptr = payload.as_ptr() as *mut c_void;
        unsafe { utp_write(self.c_socket,payload_ptr,payload.len() as u64) as usize }
    }

    pub async fn receive(&self) -> Result<Vec<u8>,UtpError> {
        loop {
            if let Some(res) = self.to_read.read.lock().await.recv().await {
                break Ok(res);
            } else {
                println!("receive finished because socket is closed");
                break Err(UtpError::SocketDestroyed);
            }
        }
    }

}
