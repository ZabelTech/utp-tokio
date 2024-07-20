use utp_tokio::utp::*;
use std::str;

#[tokio::test]
async fn test_case_send_recv_connected() {
    let send_addr  = "127.0.0.1:8123".parse().ok();
    let send_ctx   = UtpCtx::new(send_addr).await;
    let recv_addr  = "127.0.0.1:8124".parse().ok();
    let recv_ctx   = UtpCtx::new(recv_addr).await;
    let sender     = send_ctx.connect(recv_addr.unwrap()).await;
    let accepted   = recv_ctx.accept().await;

    let msg1 = "hello receiver!";
    sender.write(msg1.as_bytes()).await;
    let res1 = accepted.receive().await;
    assert_eq!(Ok(msg1),str::from_utf8(&res1));

    let msg2 = "hello back!";
    accepted.write(msg2.as_bytes()).await;
    let res2 = sender.receive().await;
    assert_eq!(Ok(msg2),str::from_utf8(&res2));


    let msg3 = "hello back again!";
    accepted.write(msg3.as_bytes()).await;
    let res3 = sender.receive().await;
    assert_eq!(Ok(msg3),str::from_utf8(&res3));

    let msg4 = "hello again";
    sender.write(msg4.as_bytes()).await;
    let res4 = accepted.receive().await;
    assert_eq!(Ok(msg4),str::from_utf8(&res4));
}
