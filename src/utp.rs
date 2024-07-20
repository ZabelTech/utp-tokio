use std::ffi::{CStr,c_void};
use std::net::SocketAddr;
use std::slice::from_raw_parts;
use std::sync::Arc;
use os_socketaddr::OsSocketAddr;

use tokio::net::UdpSocket;
use tokio::sync::{Mutex,Notify,mpsc};
use tokio::time::{Duration,sleep};

use libutp_sys::*;

#[derive(Debug)]
struct Buffer<T> {
    read:  Arc<Mutex<mpsc::Receiver<T>>>,
    write: mpsc::Sender<T>
}

#[derive(Debug)]
pub struct UtpSocket {
    utp_socket: *mut utp_socket,
    to_read:    Buffer<Vec<u8>>,
}

unsafe impl Send for UtpSocket {}
unsafe impl Sync for UtpSocket {}

#[derive(Debug)]
pub struct UtpCtx {
    incomming: Buffer<Arc<UtpSocket>>,
    socket:    Arc<UdpSocket>,
    ctx:       *mut utp_context,
}
unsafe impl Send for UtpCtx {}
unsafe impl Sync for UtpCtx {}

impl UtpCtx {
    pub async fn new(address: SocketAddr) -> Arc<UtpCtx> {
        let c_ctx = unsafe { utp_init(2) };

        unsafe {
            utp_set_callback(c_ctx,UTP_ON_READ as i32,Some(on_read));
            utp_set_callback(c_ctx,UTP_SENDTO as i32,Some(on_sendto));
            utp_set_callback(c_ctx,UTP_ON_CONNECT as i32,Some(on_connect));
            utp_set_callback(c_ctx,UTP_ON_ACCEPT as i32,Some(on_accept));
            utp_set_callback(c_ctx,UTP_ON_STATE_CHANGE as i32,Some(on_state_change));
            utp_set_callback(c_ctx,UTP_ON_ERROR as i32,Some(on_error));
            utp_set_callback(c_ctx,UTP_LOG as i32,Some(log));
            utp_context_set_option(c_ctx, UTP_LOG_DEBUG as i32,  3);
            utp_context_set_option(c_ctx, UTP_LOG_NORMAL as i32, 3);
            utp_context_set_option(c_ctx, UTP_LOG_MTU as i32,    1);
        }

        let listener = UdpSocket::bind(address).await;
        if let Err(err) = listener {
            panic!("couldn't bind to {:?} because of {:?}",address,err);
        }
        let listener = Arc::new(listener.unwrap());

        if let Err(err) = listener.writable().await {
            panic!("socket {:?} didn't become writable {:?}",address,err);
        }

        let (write,read) = mpsc::channel(100);
        let incomming    = Buffer {write, read : Arc::new(Mutex::new(read))};

        let ctx = Arc::new(UtpCtx { incomming, socket: listener.clone(), ctx: c_ctx });

        unsafe { utp_context_set_userdata(c_ctx, Arc::into_raw(ctx.clone()) as *mut c_void); }

        let _handle = tokio::spawn(utp_listener(ctx.clone(),listener.clone()));

        let child_ctx = ctx.clone();
        let _handle = tokio::spawn(async move {
            loop {
                unsafe { utp_check_timeouts(child_ctx.ctx) }
                sleep(Duration::from_millis(500)).await;
            }
        });

        ctx
    }

    pub async fn accept(&self) -> Arc<UtpSocket> {
        let Some(socket) = self.incomming.read.lock().await.recv().await else {
            panic!("error accepting")
        };
        socket
    }

    pub async fn connect(&self, addr: SocketAddr) -> Arc<UtpSocket> {
        let utp_socket = unsafe { utp_create_socket(self.ctx)};
        let on_connect = Arc::new(Notify::new());

        unsafe {
            utp_set_userdata(utp_socket, Arc::into_raw(on_connect.clone()) as *mut c_void);
        }

        let addr : OsSocketAddr = addr.into();
        if unsafe { utp_connect(utp_socket, addr.as_ptr(), addr.len()) } != 0 {
            panic!("couldn't connect to: {:?}", addr)
        };

        on_connect.notified().await;

        UtpSocket::new(utp_socket)
    }

}

#[no_mangle]
pub unsafe extern "C" fn on_read(args: *mut utp_callback_arguments) -> u64 {
    let utp     = &*(utp_get_userdata((*args).socket) as *const UtpSocket);
    let payload = from_raw_parts((*args).buf, (*args).len as usize);

    if let Err(err) = utp.to_read.write.try_send(payload.to_vec()) {
        println!("error reading: {:?} {:?}",utp,err);
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
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn on_connect(args: *mut utp_callback_arguments) -> u64 {
    let on_connect = &*(utp_get_userdata((*args).socket) as *const Notify);
    on_connect.notify_waiters();
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_accept(args: *mut utp_callback_arguments) -> u64 {
    let ctx        = &*(utp_context_get_userdata((*args).context) as *const UtpCtx);
    let utp_socket = UtpSocket::new((*args).socket);

    if let Err(err) = ctx.incomming.write.try_send(utp_socket) {
        println!("error accpeting: {:?}",err);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn on_state_change(_args: *mut utp_callback_arguments) -> u64 {
    println!("on state change");
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
    let str = CStr::from_ptr((*args).buf as *const i8);
    println!("on error: {:?}",str);
    0
}

async fn utp_listener(ctx: Arc<UtpCtx>, socket: Arc<UdpSocket>) {
    loop {
        let mut buffer = vec![0; 1024];
        let (n_bytes, remote_address) = socket
            .recv_from(&mut buffer)
            .await
            .expect("can't read from incomming udp connection");

        let c_addr : OsSocketAddr = remote_address.into();
        { let ctx = ctx.ctx;
            let ret = unsafe {
                utp_process_udp(ctx,buffer.as_ptr(),n_bytes as u64,c_addr.as_ptr(),c_addr.len())
            };

            unsafe { utp_issue_deferred_acks(ctx); }

            if ret != 1 {
                println!("got non utp packet")
            }
        };
    }
}

impl UtpSocket {
    pub fn new(utp_socket: *mut utp_socket) -> Arc<UtpSocket> {
        let (write,read) = mpsc::channel(100);
        let to_read      = Buffer {write, read : Arc::new(Mutex::new(read))};

        let utp = Arc::new(UtpSocket{utp_socket,to_read});

        unsafe {
            utp_set_userdata(utp_socket, Arc::into_raw(utp.clone()) as *mut c_void);
        }
        utp
    }

    pub async fn write(&self, payload: &[u8]) -> usize {
        let payload_ptr = payload.as_ptr() as *mut c_void;
        unsafe { utp_write(self.utp_socket,payload_ptr,payload.len() as u64) as usize }
    }

    pub async fn receive(&self) -> Vec<u8> {
        loop {
            if let Some(res) = self.to_read.read.lock().await.recv().await {
                break res;
            }
        }
    }

}
