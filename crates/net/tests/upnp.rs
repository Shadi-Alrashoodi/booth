use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use net::upnp::{
    Action, Fault, Ignored, Kind, LEASE, MAX_ANSWER, Mapped, Owner, Search, Service, UpnpError,
    Url, add_mapping, delete_mapping, describe, external_ip, find, http_response, mapping_entry,
    services, soap_answer, ssdp_answer,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const HOME_GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 1);
const OUR_LAN_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 168, 100, 38), 41062);

// TP-Link style: IGD 1, port 1900, UPnP 1.0 with a URLBase, short control
// paths, and the Layer3Forwarding and WANCommonInterfaceConfig services that
// are not ours to use.
const TPLINK_STYLE: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
<specVersion>
<major>1</major>
<minor>0</minor>
</specVersion>
<URLBase>http://192.168.0.1:1900</URLBase>
<device>
<deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
<presentationURL>http://192.168.0.1:80</presentationURL>
<friendlyName>Wireless Router AC1200</friendlyName>
<manufacturer>Router maker</manufacturer>
<modelName>AC1200</modelName>
<modelNumber>3.0</modelNumber>
<UDN>uuid:9f0865b3-f5da-4ad5-85b7-7404637fdf37</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
<serviceId>urn:upnp-org:serviceId:L3Forwarding1</serviceId>
<controlURL>/l3f</controlURL>
<eventSubURL>/l3f</eventSubURL>
<SCPDURL>/l3f.xml</SCPDURL>
</service>
</serviceList>
<deviceList>
<device>
<deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
<friendlyName>WAN Device</friendlyName>
<UDN>uuid:9f0865b3-f5da-4ad5-85b7-7404637fdf38</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>
<serviceId>urn:upnp-org:serviceId:WANCommonInterfaceConfig</serviceId>
<controlURL>/ifc</controlURL>
<eventSubURL>/ifc</eventSubURL>
<SCPDURL>/ifc.xml</SCPDURL>
</service>
</serviceList>
<deviceList>
<device>
<deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
<friendlyName>WAN Connection Device</friendlyName>
<UDN>uuid:9f0865b3-f5da-4ad5-85b7-7404637fdf39</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
<serviceId>urn:upnp-org:serviceId:WANIPConnection</serviceId>
<controlURL>/ipc</controlURL>
<eventSubURL>/ipc</eventSubURL>
<SCPDURL>/ipc.xml</SCPDURL>
</service>
</serviceList>
</device>
</deviceList>
</device>
</deviceList>
</device>
</root>
"#;

// Fritz style: IGD 2 on port 49000, long control paths, a vendor service
// and an IPv6 firewall service beside the one we want.
const FRITZ_STYLE: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
	<specVersion>
		<major>1</major>
		<minor>0</minor>
	</specVersion>
	<device>
		<deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:2</deviceType>
		<friendlyName>Home Box 7590</friendlyName>
		<manufacturer>Box maker</manufacturer>
		<manufacturerURL>http://www.example.com</manufacturerURL>
		<modelName>Home Box 7590</modelName>
		<UDN>uuid:75802409-bccb-40e7-8e6c-3431c4e5a8b7</UDN>
		<iconList>
			<icon>
				<mimetype>image/gif</mimetype>
				<width>118</width>
				<height>119</height>
				<depth>8</depth>
				<url>/ligd.gif</url>
			</icon>
		</iconList>
		<serviceList>
			<service>
				<serviceType>urn:schemas-any-com:service:Any:1</serviceType>
				<serviceId>urn:any-com:serviceId:any1</serviceId>
				<controlURL>/igdupnp/control/any</controlURL>
				<eventSubURL>/igdupnp/control/any</eventSubURL>
				<SCPDURL>/any.xml</SCPDURL>
			</service>
		</serviceList>
		<deviceList>
			<device>
				<deviceType>urn:schemas-upnp-org:device:WANDevice:2</deviceType>
				<friendlyName>WANDevice - Home Box 7590</friendlyName>
				<UDN>uuid:76802409-bccb-40e7-8e6b-3431c4e5a8b7</UDN>
				<serviceList>
					<service>
						<serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>
						<serviceId>urn:upnp-org:serviceId:WANCommonIFC1</serviceId>
						<controlURL>/igd2upnp/control/WANCommonIFC1</controlURL>
						<eventSubURL>/igd2upnp/control/WANCommonIFC1</eventSubURL>
						<SCPDURL>/igd2icfgSCPD.xml</SCPDURL>
					</service>
				</serviceList>
				<deviceList>
					<device>
						<deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:2</deviceType>
						<friendlyName>WANConnectionDevice - Home Box 7590</friendlyName>
						<UDN>uuid:76802409-bccb-40e7-8e6a-3431c4e5a8b7</UDN>
						<serviceList>
							<service>
								<serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>
								<serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
								<controlURL>/igd2upnp/control/WANIPConn1</controlURL>
								<eventSubURL>/igd2upnp/control/WANIPConn1</eventSubURL>
								<SCPDURL>/igd2ipconnSCPD.xml</SCPDURL>
							</service>
							<service>
								<serviceType>urn:schemas-upnp-org:service:WANIPv6FirewallControl:1</serviceType>
								<serviceId>urn:upnp-org:serviceId:WANIPv6Firewall1</serviceId>
								<controlURL>/igd2upnp/control/WANIPv6Firewall1</controlURL>
								<eventSubURL>/igd2upnp/control/WANIPv6Firewall1</eventSubURL>
								<SCPDURL>/igd2ip6fwSCPD.xml</SCPDURL>
							</service>
						</serviceList>
					</device>
				</deviceList>
			</device>
		</deviceList>
		<presentationURL>http://box.home</presentationURL>
	</device>
</root>
"#;

// A DSL modem with a PPP session: a namespace prefix on every element, a
// comment, CDATA, an entity, a self-closing element, a service type split
// over lines, a relative control URL without a leading slash, and an IP
// connection whose control URL is on another host.
const PPP_STYLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- written by the modem's upnp daemon -->
<ns0:root xmlns:ns0="urn:schemas-upnp-org:device-1-0">
  <ns0:specVersion><ns0:major>1</ns0:major><ns0:minor>0</ns0:minor></ns0:specVersion>
  <ns0:device>
    <ns0:deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</ns0:deviceType>
    <ns0:friendlyName>DSL modem &amp; router</ns0:friendlyName>
    <ns0:presentationURL/>
    <ns0:deviceList>
      <ns0:device>
        <ns0:deviceType>urn:schemas-upnp-org:device:WANDevice:1</ns0:deviceType>
        <ns0:deviceList>
          <ns0:device>
            <ns0:deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</ns0:deviceType>
            <ns0:serviceList>
              <ns0:service>
                <ns0:serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</ns0:serviceType>
                <ns0:serviceId>urn:upnp-org:serviceId:WANIPConn1</ns0:serviceId>
                <ns0:controlURL>http://10.0.0.99:5431/control/WANIPConnection1</ns0:controlURL>
                <ns0:SCPDURL>/ipconn.xml</ns0:SCPDURL>
              </ns0:service>
              <!-- the session the modem actually dials -->
              <ns0:service>
                <ns0:serviceType>
                  urn:schemas-upnp-org:service:WANPPPConnection:1
                </ns0:serviceType>
                <ns0:serviceId>urn:upnp-org:serviceId:WANPPPConn1</ns0:serviceId>
                <ns0:controlURL><![CDATA[control/WANPPPConnection1]]></ns0:controlURL>
                <ns0:eventSubURL>event/WANPPPConnection1</ns0:eventSubURL>
                <ns0:SCPDURL>pppconn.xml</ns0:SCPDURL>
              </ns0:service>
            </ns0:serviceList>
          </ns0:device>
        </ns0:deviceList>
      </ns0:device>
    </ns0:deviceList>
  </ns0:device>
</ns0:root>
"#;

fn url(text: &str, gateway: Ipv4Addr) -> Url {
    Url::parse(text, gateway).unwrap_or_else(|| panic!("{text} did not parse"))
}

fn service(kind: Kind, text: &str, gateway: Ipv4Addr) -> Service {
    Service {
        kind,
        control: url(text, gateway),
    }
}

fn scan(xml: &str) -> Result<Vec<Service>, UpnpError> {
    services(
        xml.as_bytes(),
        &url("http://192.168.0.1:5000/desc.xml", HOME_GATEWAY),
    )
}

fn one_service(control: &str) -> String {
    format!(
        "<root><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>{control}</controlURL></service></serviceList></device></root>"
    )
}

fn ssdp(location: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nUSN: uuid:9f0865b3-f5da-4ad5-85b7-7404637fdf37::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nEXT:\r\nSERVER: Linux/3.14, UPnP/1.0, router/1.0\r\nLOCATION: {location}\r\n\r\n"
    )
}

fn from_gateway() -> SocketAddr {
    SocketAddr::from((HOME_GATEWAY, 1900))
}

fn soap_ok(action: &str, values: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">{values}</u:{action}Response></s:Body></s:Envelope>\r\n"
    )
}

fn soap_fault(code: &str, name: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{name}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>\r\n"
    )
}

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\nServer: router/1.0 UPnP/1.0\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn chunked(body: &str, size: usize) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for piece in body.as_bytes().chunks(size) {
        out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        out.extend_from_slice(piece);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

// The text of <name>...</name> in a request we sent.
fn arg<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = request.find(&open)? + open.len();
    let end = request[start..].find(&format!("</{name}>"))? + start;
    Some(&request[start..end])
}

fn soap_action(request: &str) -> &str {
    request
        .lines()
        .find_map(|line| line.strip_prefix("SOAPAction: "))
        .and_then(|value| value.trim_matches('"').split('#').nth(1))
        .unwrap_or_default()
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let len = stream.read(&mut buf).unwrap_or(0);
        if len == 0 {
            break;
        }
        data.extend_from_slice(&buf[..len]);
        let text = String::from_utf8_lossy(&data);
        if let Some(head) = text.find("\r\n\r\n") {
            let length = text[..head]
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .map_or(0, |value| value.trim().parse().unwrap());
            if data.len() >= head + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(data).unwrap()
}

// A router's web server on loopback: one connection per request, each
// answered by `answer`, and every request handed back to the test.
fn fake_router(
    connections: usize,
    mut answer: impl FnMut(&str) -> Vec<u8> + Send + 'static,
) -> (SocketAddrV4, mpsc::Receiver<String>) {
    let listener = TcpListener::bind((LOCALHOST, 0)).unwrap();
    let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
        panic!("bound an ipv4 address");
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let reply = answer(&request);
            let _ = tx.send(request);
            let _ = stream.write_all(&reply);
        }
    });
    (addr, rx)
}

fn requests(rx: &mpsc::Receiver<String>) -> Vec<String> {
    rx.try_iter().collect()
}

fn ip_service(addr: SocketAddrV4) -> Service {
    service(
        Kind::WanIp1,
        &format!("http://{addr}/ctl/IPConn"),
        LOCALHOST,
    )
}

// An SSDP responder on loopback. It answers the first search with
// `answers`, sent back to the searching socket, then takes the second, and
// hands both searches back.
fn fake_ssdp(answers: Vec<String>) -> (SocketAddr, mpsc::Receiver<Vec<String>>) {
    let socket = UdpSocket::bind((LOCALHOST, 0)).unwrap();
    let addr = socket.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut searches = Vec::new();
        let mut buf = [0u8; 2048];
        while searches.len() < 2 {
            let Ok((len, from)) = socket.recv_from(&mut buf) else {
                break;
            };
            searches.push(String::from_utf8_lossy(&buf[..len]).into_owned());
            if searches.len() == 1 {
                for answer in &answers {
                    let _ = socket.send_to(answer.as_bytes(), from);
                }
            }
        }
        let _ = tx.send(searches);
    });
    (addr, rx)
}

#[test]
fn tplink_style_description() {
    let location = url("http://192.168.0.1:1900/gatedesc.xml", HOME_GATEWAY);
    assert_eq!(
        services(TPLINK_STYLE.as_bytes(), &location).unwrap(),
        vec![service(
            Kind::WanIp1,
            "http://192.168.0.1:1900/ipc",
            HOME_GATEWAY
        )]
    );
}

#[test]
fn fritz_style_description() {
    let gateway = Ipv4Addr::new(192, 168, 178, 1);
    let location = url("http://192.168.178.1:49000/igd2desc.xml", gateway);
    let expected = vec![service(
        Kind::WanIp2,
        "http://192.168.178.1:49000/igd2upnp/control/WANIPConn1",
        gateway,
    )];
    assert_eq!(
        services(FRITZ_STYLE.as_bytes(), &location).unwrap(),
        expected
    );
    let crlf = FRITZ_STYLE.replace('\n', "\r\n");
    assert_eq!(services(crlf.as_bytes(), &location).unwrap(), expected);
}

#[test]
fn ppp_style_description() {
    let gateway = Ipv4Addr::new(10, 0, 0, 138);
    let location = url("http://10.0.0.138:5431/igd/desc.xml", gateway);
    assert_eq!(
        services(PPP_STYLE.as_bytes(), &location).unwrap(),
        vec![service(
            Kind::WanPpp1,
            "http://10.0.0.138:5431/igd/control/WANPPPConnection1",
            gateway
        )]
    );
}

#[test]
fn control_urls_resolve_against_the_description() {
    let control = |text: &str| scan(&one_service(text)).map(|found| found[0].control.to_string());
    assert_eq!(control("/ipc").unwrap(), "http://192.168.0.1:5000/ipc");
    assert_eq!(control("ipc").unwrap(), "http://192.168.0.1:5000/ipc");
    assert_eq!(control("  /ipc\n").unwrap(), "http://192.168.0.1:5000/ipc");
    assert_eq!(
        control("http://192.168.0.1:5431/ctl").unwrap(),
        "http://192.168.0.1:5431/ctl"
    );
    assert_eq!(
        control("//192.168.0.1:6000/ctl").unwrap(),
        "http://192.168.0.1:6000/ctl"
    );
    assert_eq!(
        control("/ctl?a=1&amp;b=2").unwrap(),
        "http://192.168.0.1:5000/ctl?a=1&b=2"
    );
    assert_eq!(control("&#x2F;ipc").unwrap(), "http://192.168.0.1:5000/ipc");
    assert_eq!(control("/ipc#top").unwrap(), "http://192.168.0.1:5000/ipc");

    let nested = services(
        one_service("ctl/ip").as_bytes(),
        &url("http://192.168.0.1:5000/dev/desc.xml?v=2", HOME_GATEWAY),
    )
    .unwrap();
    assert_eq!(
        nested[0].control.to_string(),
        "http://192.168.0.1:5000/dev/ctl/ip"
    );

    // Everything that leads off the gateway, or cannot go in a request line,
    // is dropped, which leaves no service.
    for bad in [
        "http://192.168.0.2/ctl",
        "http://192.168.0.10/ctl",
        "http://192.168.0.1.example.com/ctl",
        "http://192.168.0.1@203.0.113.9/ctl",
        "http://user@192.168.0.1/ctl",
        "http://router.home/ctl",
        "http://[::1]/ctl",
        "http://192.168.0.1:0/ctl",
        "http://192.168.0.1:70000/ctl",
        "https://192.168.0.1/ctl",
        "//203.0.113.9/ctl",
        "javascript:alert(1)",
        "uuid:1234/WANIPConnection:1",
        "/ctl with space",
        "/ctl\u{e9}",
        "",
    ] {
        assert!(
            matches!(scan(&one_service(bad)), Err(UpnpError::NoService(_))),
            "{bad:?} was not dropped"
        );
    }
}

#[test]
fn url_base_elsewhere_is_ignored() {
    let xml = TPLINK_STYLE.replace("http://192.168.0.1:1900", "http://203.0.113.9:1900");
    let location = url("http://192.168.0.1:1900/gatedesc.xml", HOME_GATEWAY);
    assert_eq!(
        services(xml.as_bytes(), &location).unwrap()[0].control,
        url("http://192.168.0.1:1900/ipc", HOME_GATEWAY)
    );
}

#[test]
fn descriptions_it_cannot_follow_are_refused() {
    let entity_bomb = format!(
        "<?xml version=\"1.0\"?><!DOCTYPE root [<!ENTITY a \"aaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;\">]>{}",
        one_service("/ipc&b;")
    );
    let deep = format!("{}{}", "<a>".repeat(100), "</a>".repeat(100));
    let not_utf8 = [
        b"<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ip".as_slice(),
        &[0xff, 0xfe],
        b"</controlURL></service></root>",
    ]
    .concat();
    let bad: Vec<Vec<u8>> = vec![
        entity_bomb.into_bytes(),
        deep.into_bytes(),
        not_utf8,
        b"<root><device>".to_vec(),
        b"<root></device>".to_vec(),
        b"</root>".to_vec(),
        b"<root><!-- never closed".to_vec(),
        b"<root><![CDATA[never closed</root>".to_vec(),
        b"<root><?pi never closed</root>".to_vec(),
        b"<root attr=\"never closed></root>".to_vec(),
        b"<root><service><service></service></service></root>".to_vec(),
        b"<root><>".to_vec(),
        b"<root><a<b></root>".to_vec(),
        one_service("/ipc&nbsp;").into_bytes(),
        one_service("/ipc&amp").into_bytes(),
        one_service("/ipc&#xD800;").into_bytes(),
        one_service("/i<b>p</b>c").into_bytes(),
        one_service(&"/x".repeat(600)).into_bytes(),
    ];
    for xml in bad {
        let got = services(&xml, &url("http://192.168.0.1:5000/desc.xml", HOME_GATEWAY));
        assert!(
            matches!(got, Err(UpnpError::BadXml(_))),
            "{:?} gave {got:?}",
            String::from_utf8_lossy(&xml)
        );
    }

    let huge = format!("<root>{}</root>", " ".repeat(MAX_ANSWER));
    assert!(matches!(scan(&huge), Err(UpnpError::TooLarge)));
    assert!(matches!(scan(""), Err(UpnpError::NoService(_))));
    assert!(matches!(
        scan(
            "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType></service></root>"
        ),
        Err(UpnpError::NoService(_))
    ));
}

#[test]
fn descriptions_with_odd_but_valid_markup() {
    let xml = r#"<root><serviceList><service note="a > b" other='"'>
        <serviceType><![CDATA[urn:schemas-upnp-org:service:]]>WANIPConnection:1</serviceType>
        <CONTROLURL>/ipc</CONTROLURL>
        <extra/>
    </service ></serviceList></root>"#;
    assert_eq!(
        scan(xml).unwrap(),
        vec![service(
            Kind::WanIp1,
            "http://192.168.0.1:5000/ipc",
            HOME_GATEWAY
        )]
    );
}

#[test]
fn cut_off_description() {
    let location = url("http://192.168.0.1:1900/gatedesc.xml", HOME_GATEWAY);
    let whole = TPLINK_STYLE.as_bytes();
    let root_closed = TPLINK_STYLE.rfind("</root>").unwrap() + "</root>".len();
    for len in 0..root_closed {
        assert!(
            services(&whole[..len], &location).is_err(),
            "cut at {len} was read"
        );
    }
    assert!(services(&whole[..root_closed], &location).is_ok());
}

#[test]
fn ssdp_answers_from_the_gateway() {
    let good = ssdp("http://192.168.0.1:1900/gatedesc.xml");
    assert_eq!(
        ssdp_answer(good.as_bytes(), from_gateway(), HOME_GATEWAY),
        Ok(url("http://192.168.0.1:1900/gatedesc.xml", HOME_GATEWAY))
    );
    // The answer's source port is whatever the router's daemon uses.
    assert!(
        ssdp_answer(
            good.as_bytes(),
            SocketAddr::from((HOME_GATEWAY, 52001)),
            HOME_GATEWAY
        )
        .is_ok()
    );
    let mapped: SocketAddr = "[::ffff:192.168.0.1]:1900".parse().unwrap();
    assert!(ssdp_answer(good.as_bytes(), mapped, HOME_GATEWAY).is_ok());

    let terse = "HTTP/1.1 200 OK\nst: urn:schemas-upnp-org:device:InternetGatewayDevice:2\nlocation:http://192.168.0.1:5000/rootDesc.xml\n\n";
    assert_eq!(
        ssdp_answer(terse.as_bytes(), from_gateway(), HOME_GATEWAY),
        Ok(url("http://192.168.0.1:5000/rootDesc.xml", HOME_GATEWAY))
    );
    let no_st = "HTTP/1.0 200 OK\r\nLocation: HTTP://192.168.0.1:5000/rootDesc.xml\r\n\r\n";
    assert!(ssdp_answer(no_st.as_bytes(), from_gateway(), HOME_GATEWAY).is_ok());
}

#[test]
fn ssdp_answers_that_are_ignored() {
    let good = ssdp("http://192.168.0.1:1900/gatedesc.xml");
    for from in [
        "192.168.0.2:1900",
        "192.168.0.10:1900",
        "127.0.0.1:1900",
        "[fe80::1]:1900",
    ] {
        assert_eq!(
            ssdp_answer(good.as_bytes(), from.parse().unwrap(), HOME_GATEWAY),
            Err(Ignored::NotFromGateway),
            "{from}"
        );
    }

    let notify = "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: upnp:rootdevice\r\nLOCATION: http://192.168.0.1:1900/gatedesc.xml\r\n\r\n";
    let not_found = good.replace("200 OK", "404 Not Found");
    for text in [notify, not_found.as_str(), "", "HTTP/1.1", "\r\n\r\n"] {
        assert_eq!(
            ssdp_answer(text.as_bytes(), from_gateway(), HOME_GATEWAY),
            Err(Ignored::NotAnAnswer),
            "{text:?}"
        );
    }

    let printer = good.replace(
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n",
        "urn:schemas-upnp-org:device:Printer:1\r\n",
    );
    assert_eq!(
        ssdp_answer(printer.as_bytes(), from_gateway(), HOME_GATEWAY),
        Err(Ignored::NotAGateway(String::from(
            "urn:schemas-upnp-org:device:Printer:1"
        )))
    );

    let no_location =
        "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";
    assert_eq!(
        ssdp_answer(no_location.as_bytes(), from_gateway(), HOME_GATEWAY),
        Err(Ignored::NoLocation)
    );

    for location in [
        "http://192.168.0.10:1900/gatedesc.xml",
        "http://192.168.0.1.example.com/gatedesc.xml",
        "http://192.168.0.1@203.0.113.9/gatedesc.xml",
        "https://192.168.0.1/gatedesc.xml",
        "http://router.home:1900/gatedesc.xml",
        "http://0300.0250.0.1/gatedesc.xml",
        "http://192.168.000.001/gatedesc.xml",
        "http://3232235521/gatedesc.xml",
        "ftp://192.168.0.1/gatedesc.xml",
        "/gatedesc.xml",
        "",
    ] {
        assert!(
            matches!(
                ssdp_answer(ssdp(location).as_bytes(), from_gateway(), HOME_GATEWAY),
                Err(Ignored::LocationElsewhere(_) | Ignored::NoLocation)
            ),
            "{location:?} was taken"
        );
    }

    // Whoever answers picks what the host fetches from the router, and a
    // query is what its admin pages would need.
    let admin = ssdp("http://192.168.0.1:80/cgi-bin/luci/admin/system/reboot?confirm=1");
    assert_eq!(
        ssdp_answer(admin.as_bytes(), from_gateway(), HOME_GATEWAY),
        Err(Ignored::LocationQuery(String::from(
            "http://192.168.0.1:80/cgi-bin/luci/admin/system/reboot?confirm=1"
        )))
    );

    // What goes to the log cannot carry control characters.
    let sneaky = ssdp("http://203.0.113.9/\u{1b}[31m\u{7f}x");
    let Err(Ignored::LocationElsewhere(shown)) =
        ssdp_answer(sneaky.as_bytes(), from_gateway(), HOME_GATEWAY)
    else {
        panic!("sneaky location was not refused");
    };
    assert!(
        shown.chars().all(|c| c.is_ascii_graphic() || c == ' '),
        "{shown:?}"
    );
}

#[test]
fn urls() {
    let plain = url("http://192.168.0.1", HOME_GATEWAY);
    assert_eq!(plain.addr(), SocketAddrV4::new(HOME_GATEWAY, 80));
    assert_eq!(plain.path(), "/");
    assert_eq!(plain.to_string(), "http://192.168.0.1:80/");
    let full = url(
        " HTTP://192.168.0.1:5000/rootDesc.xml?x=1#frag ",
        HOME_GATEWAY,
    );
    assert_eq!(full.addr(), SocketAddrV4::new(HOME_GATEWAY, 5000));
    assert_eq!(full.path(), "/rootDesc.xml?x=1");
    assert_eq!(Url::parse(&full.to_string(), HOME_GATEWAY), Some(full));
    let long = format!("http://192.168.0.1/{}", "a".repeat(600));
    assert_eq!(Url::parse(&long, HOME_GATEWAY), None);
    assert_eq!(Url::parse("http://192.168.0.1:/x", HOME_GATEWAY), None);
    assert_eq!(Url::parse("http://192.168.0.1:80:80/x", HOME_GATEWAY), None);
    assert_eq!(Url::parse("http://192.168.0.1?x", HOME_GATEWAY), None);
}

#[test]
fn http_framing() {
    let whole = http("200 OK", "hello");
    let got = http_response(&whole, false).unwrap().unwrap();
    assert_eq!(
        (got.status, got.body.as_slice()),
        (200, b"hello".as_slice())
    );
    for len in 0..whole.len() {
        assert_eq!(
            http_response(&whole[..len], false).unwrap(),
            None,
            "cut at {len}"
        );
        assert!(http_response(&whole[..len], true).is_err(), "cut at {len}");
    }

    let chunks = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\nContent-Length: 3\r\n\r\n5;name=value\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n";
    let got = http_response(chunks, false).unwrap().unwrap();
    assert_eq!(got.body, b"hello world");
    for len in 0..chunks.len() {
        assert_eq!(
            http_response(&chunks[..len], false).unwrap(),
            None,
            "cut at {len}"
        );
    }

    let to_close = b"HTTP/1.0 200 OK\r\nServer: tiny\r\n\r\n<xml/>";
    assert_eq!(http_response(to_close, false).unwrap(), None);
    assert_eq!(
        http_response(to_close, true).unwrap().unwrap().body,
        b"<xml/>"
    );

    let bare_lf = b"HTTP/1.1 500 Internal Server Error\nContent-Length: 2\n\nab";
    let got = http_response(bare_lf, false).unwrap().unwrap();
    assert_eq!((got.status, got.body.as_slice()), (500, b"ab".as_slice()));

    let big = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        MAX_ANSWER + 1
    );
    assert!(matches!(
        http_response(big.as_bytes(), false),
        Err(UpnpError::TooLarge)
    ));
    let big_chunk = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffff\r\n";
    assert!(matches!(
        http_response(big_chunk, false),
        Err(UpnpError::TooLarge)
    ));
    assert!(matches!(
        http_response(&vec![b'a'; MAX_ANSWER + 1], false),
        Err(UpnpError::TooLarge)
    ));

    for broken in [
        b"HTTP/2 200 OK\r\n\r\n".as_slice(),
        b"HTTP/1.1 20 OK\r\n\r\n",
        b"HTTP/1.1 +20 OK\r\n\r\n",
        b"ICY 200 OK\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc",
        b"HTTP/1.1 200 OK\r\nContent-Length: two\r\n\r\nab",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\nab",
        b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX: \xff\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n123456789\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXY",
    ] {
        assert!(
            matches!(http_response(broken, false), Err(UpnpError::BadHttp(_))),
            "{:?}",
            String::from_utf8_lossy(broken)
        );
    }
}

#[test]
fn soap_values_and_faults() {
    let action = Action::GetExternalIpAddress;
    let ok = soap_ok(
        "GetExternalIPAddress",
        "<NewExternalIPAddress> 203.0.113.7 </NewExternalIPAddress>",
    );
    let values = soap_answer(action, 200, ok.as_bytes()).unwrap();
    assert_eq!(values.get("NewExternalIPAddress"), Some("203.0.113.7"));
    assert_eq!(values.get("newexternalipaddress"), Some("203.0.113.7"));
    assert_eq!(values.get("NewInternalClient"), None);

    let entry = soap_ok(
        "GetSpecificPortMappingEntry",
        "<NewInternalPort>41062</NewInternalPort><NewInternalClient><![CDATA[192.168.100.38]]></NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>Booth &amp; co</NewPortMappingDescription><NewLeaseDuration/>",
    );
    let values = soap_answer(Action::GetSpecificPortMappingEntry, 200, entry.as_bytes()).unwrap();
    assert_eq!(values.get("NewInternalClient"), Some("192.168.100.38"));
    assert_eq!(values.get("NewPortMappingDescription"), Some("Booth & co"));
    assert_eq!(values.get("NewLeaseDuration"), Some(""));

    let add = Action::AddPortMapping;
    let conflict = soap_fault("718", "ConflictInMappingEntry");
    let got = soap_answer(add, 500, conflict.as_bytes());
    assert!(matches!(
        got,
        Err(UpnpError::Fault(Action::AddPortMapping, Fault::CONFLICT))
    ));
    assert_eq!(
        got.unwrap_err().to_string(),
        "the router refused AddPortMapping with error 718 ConflictInMappingEntry"
    );
    // Some routers send the fault with 200.
    assert!(matches!(
        soap_answer(add, 200, conflict.as_bytes()),
        Err(UpnpError::Fault(_, Fault::CONFLICT))
    ));
    let permanent = soap_fault(" 725\n", "OnlyPermanentLeasesSupported");
    assert!(matches!(
        soap_answer(add, 500, permanent.as_bytes()),
        Err(UpnpError::Fault(_, Fault::ONLY_PERMANENT_LEASES))
    ));
    assert!(matches!(
        soap_answer(add, 500, soap_fault("seven", "x").as_bytes()),
        Err(UpnpError::BadAnswer(_, "error code"))
    ));

    assert!(matches!(
        soap_answer(add, 500, b""),
        Err(UpnpError::Status(500))
    ));
    assert!(matches!(
        soap_answer(add, 401, b"<html><body>Log in first<br></body></html>"),
        Err(UpnpError::Status(401))
    ));
    assert!(matches!(
        soap_answer(add, 200, b"<html><body>Log in first</body></html>"),
        Err(UpnpError::BadAnswer(_, "response element"))
    ));
    assert!(matches!(
        soap_answer(add, 200, soap_ok("DeletePortMapping", "").as_bytes()),
        Err(UpnpError::BadAnswer(_, "response element"))
    ));
    assert!(matches!(
        soap_answer(add, 200, b"<a><b>"),
        Err(UpnpError::BadXml(_))
    ));
    assert!(soap_answer(add, 200, soap_ok("AddPortMapping", "").as_bytes()).is_ok());
}

#[test]
fn fault_names() {
    assert_eq!(Fault::CONFLICT.name(), Some("ConflictInMappingEntry"));
    assert_eq!(
        Fault::ONLY_PERMANENT_LEASES.name(),
        Some("OnlyPermanentLeasesSupported")
    );
    assert_eq!(Fault::NO_SUCH_ENTRY.name(), Some("NoSuchEntryInArray"));
    assert_eq!(Fault(606).name(), Some("ActionNotAuthorized"));
    assert_eq!(Fault(999).name(), None);
    assert_eq!(Fault(606).to_string(), "606 ActionNotAuthorized");
    assert_eq!(Fault(999).to_string(), "999");
}

#[test]
fn external_ip_over_http() {
    let (addr, rx) = fake_router(3, |request| {
        let value = match request.lines().next().unwrap_or_default() {
            "POST /ctl/IPConn HTTP/1.1" => "203.0.113.7",
            "POST /ctl/Down HTTP/1.1" => "",
            _ => "0.0.0.0",
        };
        http(
            "200 OK",
            &soap_ok(
                "GetExternalIPAddress",
                &format!("<NewExternalIPAddress>{value}</NewExternalIPAddress>"),
            ),
        )
    });
    assert_eq!(
        external_ip(&ip_service(addr)).unwrap(),
        Ipv4Addr::new(203, 0, 113, 7)
    );
    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("POST /ctl/IPConn HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains(&format!("\r\nHost: {addr}\r\n")),
        "{request}"
    );
    assert!(request.contains(
        "\r\nSOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress\"\r\n"
    ));
    assert!(request.contains("\r\nConnection: close\r\n"));
    let (head, body) = request.split_once("\r\n\r\n").unwrap();
    assert!(head.contains(&format!("Content-Length: {}", body.len())));
    assert!(body.contains(
        "<u:GetExternalIPAddress xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\"></u:GetExternalIPAddress>"
    ));

    for path in ["/ctl/Down", "/ctl/Zero"] {
        let down = service(Kind::WanPpp1, &format!("http://{addr}{path}"), LOCALHOST);
        assert!(matches!(
            external_ip(&down),
            Err(UpnpError::BadAnswer(Action::GetExternalIpAddress, _))
        ));
    }
}

#[test]
fn add_mapping_sends_every_argument() {
    let (addr, rx) = fake_router(2, |request| match soap_action(request) {
        "GetSpecificPortMappingEntry" => no_entry(),
        _ => http("200 OK", &soap_ok("AddPortMapping", "")),
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_eq!(
        mapped,
        Mapped {
            external_port: 41062,
            lease: LEASE,
            delete_on_close: true
        }
    );
    let sent = requests(&rx);
    assert_eq!(sent.len(), 2);
    assert_eq!(soap_action(&sent[0]), "GetSpecificPortMappingEntry");
    assert_eq!(arg(&sent[0], "NewExternalPort"), Some("41062"));
    assert_eq!(arg(&sent[0], "NewProtocol"), Some("UDP"));
    let request = &sent[1];
    assert_eq!(soap_action(request), "AddPortMapping");
    let expected = [
        ("NewRemoteHost", ""),
        ("NewExternalPort", "41062"),
        ("NewProtocol", "UDP"),
        ("NewInternalPort", "41062"),
        ("NewInternalClient", "192.168.100.38"),
        ("NewEnabled", "1"),
        ("NewPortMappingDescription", "Booth"),
        ("NewLeaseDuration", "7200"),
    ];
    let mut last = 0;
    for (name, value) in expected {
        assert_eq!(arg(request, name), Some(value), "{name}");
        let at = request.find(&format!("<{name}>")).unwrap();
        assert!(at > last, "{name} is out of order");
        last = at;
    }
}

fn no_entry() -> Vec<u8> {
    http(
        "500 Internal Server Error",
        &soap_fault("714", "NoSuchEntryInArray"),
    )
}

fn conflict() -> Vec<u8> {
    http(
        "500 Internal Server Error",
        &soap_fault("718", "ConflictInMappingEntry"),
    )
}

fn entry_answer(client: &str, port: u16, enabled: &str, lease: &str) -> Vec<u8> {
    entry_named(client, port, enabled, lease, "manual\r\nforward")
}

fn entry_named(client: &str, port: u16, enabled: &str, lease: &str, name: &str) -> Vec<u8> {
    http(
        "200 OK",
        &soap_ok(
            "GetSpecificPortMappingEntry",
            &format!(
                "<NewInternalPort>{port}</NewInternalPort><NewInternalClient>{client}</NewInternalClient><NewEnabled>{enabled}</NewEnabled><NewPortMappingDescription>{name}</NewPortMappingDescription><NewLeaseDuration>{lease}</NewLeaseDuration>"
            ),
        ),
    )
}

// A forward the user set up by hand, on a router that keeps those and UPnP's
// in one table. AddPortMapping would replace it, and Close room would then
// delete it.
#[test]
fn hand_made_forward_is_used() {
    let (addr, rx) = fake_router(1, |_| entry_answer("192.168.100.38", 41062, "1", "0"));
    let mut notes = Vec::new();
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |line| {
        notes.push(line.to_string())
    })
    .unwrap();
    assert_eq!(
        mapped,
        Mapped {
            external_port: 41062,
            lease: 0,
            delete_on_close: false
        }
    );
    let sent = requests(&rx);
    assert_eq!(sent.len(), 1, "no AddPortMapping over it");
    assert_eq!(soap_action(&sent[0]), "GetSpecificPortMappingEntry");
    assert!(
        notes.iter().any(|n| n.contains("already forwards")),
        "{notes:?}"
    );
    assert!(
        notes.iter().all(|n| !n.contains('\r') && !n.contains('\n')),
        "{notes:?}"
    );
}

#[test]
fn forward_made_after_the_read_is_kept() {
    let mut reads = 0;
    let (addr, rx) = fake_router(3, move |request| match soap_action(request) {
        "AddPortMapping" => conflict(),
        _ => {
            reads += 1;
            if reads == 1 {
                no_entry()
            } else {
                entry_answer("192.168.100.38", 41062, "1", "0")
            }
        }
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_eq!(
        (mapped.external_port, mapped.delete_on_close),
        (41062, false)
    );
    let actions: Vec<String> = requests(&rx)
        .iter()
        .map(|r| soap_action(r).to_owned())
        .collect();
    assert_eq!(
        actions,
        [
            "GetSpecificPortMappingEntry",
            "AddPortMapping",
            "GetSpecificPortMappingEntry"
        ]
    );
}

// Left by a run that was killed before it could delete it.
#[test]
fn booths_own_old_lease_is_taken_over() {
    let (addr, rx) = fake_router(2, |request| match soap_action(request) {
        "AddPortMapping" => http("200 OK", &soap_ok("AddPortMapping", "")),
        _ => entry_named("192.168.100.38", 41062, "1", "1200", "Booth"),
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_eq!(
        mapped,
        Mapped {
            external_port: 41062,
            lease: LEASE,
            delete_on_close: true
        }
    );
    let sent = requests(&rx);
    assert_eq!(soap_action(&sent[1]), "AddPortMapping");
    assert_eq!(arg(&sent[1], "NewExternalPort"), Some("41062"));
}

// Against the spec, but seen: the router answers a conflict even to the
// request that would only renew Booth's own entry, whose lease would then
// never start again.
#[test]
fn own_entry_the_router_will_not_renew() {
    let mut adds = 0;
    let (addr, rx) = fake_router(5, move |request| match soap_action(request) {
        "AddPortMapping" => {
            adds += 1;
            if adds == 1 {
                conflict()
            } else {
                http("200 OK", &soap_ok("AddPortMapping", ""))
            }
        }
        "DeletePortMapping" => http("200 OK", &soap_ok("DeletePortMapping", "")),
        _ => entry_named("192.168.100.38", 41062, "1", "3", "Booth"),
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_eq!(
        mapped,
        Mapped {
            external_port: 41062,
            lease: LEASE,
            delete_on_close: true
        }
    );
    let sent = requests(&rx);
    let actions: Vec<&str> = sent.iter().map(|r| soap_action(r)).collect();
    assert_eq!(
        actions,
        [
            "GetSpecificPortMappingEntry",
            "AddPortMapping",
            "GetSpecificPortMappingEntry",
            "DeletePortMapping",
            "AddPortMapping"
        ]
    );
    assert!(
        sent.iter()
            .all(|r| arg(r, "NewExternalPort") == Some("41062"))
    );
}

#[test]
fn another_pcs_port_moves_to_a_random_one() {
    let (addr, rx) = fake_router(2, |request| match soap_action(request) {
        "AddPortMapping" => http("200 OK", &soap_ok("AddPortMapping", "")),
        _ => entry_answer("192.168.100.20", 41062, "1", "0"),
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert!(
        (49152..=65535).contains(&mapped.external_port),
        "{mapped:?}"
    );
    assert_eq!((mapped.lease, mapped.delete_on_close), (LEASE, true));
    let sent = requests(&rx);
    assert_eq!(sent.len(), 2, "no AddPortMapping over another pc's port");
    assert_eq!(soap_action(&sent[1]), "AddPortMapping");
    assert_eq!(
        arg(&sent[1], "NewExternalPort"),
        Some(mapped.external_port.to_string().as_str())
    );
    assert_eq!(arg(&sent[1], "NewInternalPort"), Some("41062"));
}

#[test]
fn disabled_forward_is_not_taken() {
    let (addr, _rx) = fake_router(2, |request| match soap_action(request) {
        "AddPortMapping" => http("200 OK", &soap_ok("AddPortMapping", "")),
        _ => entry_answer("192.168.100.38", 41062, "0", "0"),
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_ne!(mapped.external_port, 41062);
    assert!(mapped.delete_on_close);
}

#[test]
fn conflict_everywhere_gives_up_after_three_random_ports() {
    let (addr, rx) = fake_router(6, |request| match soap_action(request) {
        "AddPortMapping" => conflict(),
        _ => no_entry(),
    });
    let got = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {});
    assert!(matches!(got, Err(UpnpError::NoFreePort(41062))), "{got:?}");
    let sent = requests(&rx);
    assert_eq!(sent.len(), 6);
    let mut ports: Vec<&str> = sent
        .iter()
        .filter(|r| soap_action(r) == "AddPortMapping")
        .map(|r| arg(r, "NewExternalPort").unwrap())
        .collect();
    assert_eq!(ports.len(), 4);
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), 4, "a port was tried twice");
}

#[test]
fn only_permanent_leases_retries_with_zero() {
    let (addr, rx) = fake_router(3, |request| {
        if soap_action(request) == "GetSpecificPortMappingEntry" {
            no_entry()
        } else if arg(request, "NewLeaseDuration") == Some("0") {
            http("200 OK", &soap_ok("AddPortMapping", ""))
        } else {
            http(
                "500 Internal Server Error",
                &soap_fault("725", "OnlyPermanentLeasesSupported"),
            )
        }
    });
    let mapped = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {}).unwrap();
    assert_eq!(
        mapped,
        Mapped {
            external_port: 41062,
            lease: 0,
            delete_on_close: true
        }
    );
    let sent = requests(&rx);
    assert_eq!(arg(&sent[1], "NewLeaseDuration"), Some("7200"));
    assert_eq!(arg(&sent[2], "NewLeaseDuration"), Some("0"));
}

// A router that cannot read one entry back still gets asked for the port.
#[test]
fn other_faults_stop_the_mapping() {
    let (addr, rx) = fake_router(2, |_| {
        http(
            "500 Internal Server Error",
            &soap_fault("606", "Action not authorized"),
        )
    });
    let got = add_mapping(&ip_service(addr), OUR_LAN_ADDR, 41062, &mut |_| {});
    assert!(
        matches!(
            got,
            Err(UpnpError::Fault(Action::AddPortMapping, Fault(606)))
        ),
        "{got:?}"
    );
    assert_eq!(requests(&rx).len(), 2);
}

#[test]
fn delete_and_read_entries() {
    let (addr, rx) = fake_router(3, |request| match soap_action(request) {
        "DeletePortMapping" => http("200 OK", &soap_ok("DeletePortMapping", "")),
        _ if arg(request, "NewExternalPort") == Some("41063") => http(
            "500 Internal Server Error",
            &soap_fault("714", "NoSuchEntryInArray"),
        ),
        _ => entry_answer("192.168.100.38", 41062, "1", "3600"),
    });
    let service = ip_service(addr);
    delete_mapping(&service, 41062).unwrap();
    let sent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(soap_action(&sent), "DeletePortMapping");
    assert_eq!(arg(&sent, "NewRemoteHost"), Some(""));
    assert_eq!(arg(&sent, "NewExternalPort"), Some("41062"));
    assert_eq!(arg(&sent, "NewProtocol"), Some("UDP"));

    let entry = mapping_entry(&service, 41062).unwrap();
    assert_eq!(entry.internal, OUR_LAN_ADDR);
    assert!(entry.enabled);
    assert_eq!(entry.lease, 3600);
    assert_eq!(entry.description, "manual??forward");
    assert_eq!(entry.owner(OUR_LAN_ADDR), Owner::ThisPc);
    let elsewhere = SocketAddrV4::new(*OUR_LAN_ADDR.ip(), 41063);
    assert_eq!(entry.owner(elsewhere), Owner::Other);
    let mut booths = entry.clone();
    booths.description = String::from("Booth");
    booths.enabled = false;
    assert_eq!(booths.owner(OUR_LAN_ADDR), Owner::Booth);

    let missing = mapping_entry(&service, 41063).unwrap_err();
    assert!(matches!(
        missing,
        UpnpError::Fault(Action::GetSpecificPortMappingEntry, Fault::NO_SUCH_ENTRY)
    ));
    assert!(missing.to_string().contains("714 NoSuchEntryInArray"));
}

#[test]
fn describe_reads_a_chunked_description() {
    let (addr, rx) = fake_router(1, |_| chunked(FRITZ_STYLE, 700));
    let location = url(&format!("http://{addr}/igd2desc.xml"), LOCALHOST);
    let found = describe(&location).unwrap();
    assert_eq!(
        found,
        vec![service(
            Kind::WanIp2,
            &format!("http://{addr}/igd2upnp/control/WANIPConn1"),
            LOCALHOST
        )]
    );
    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        request,
        format!("GET /igd2desc.xml HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
    );
}

#[test]
fn describe_refuses_an_error_page() {
    let (addr, _rx) = fake_router(1, |_| http("404 Not Found", "<html>no</html>"));
    let location = url(&format!("http://{addr}/rootDesc.xml"), LOCALHOST);
    assert!(matches!(describe(&location), Err(UpnpError::Status(404))));
}

#[test]
fn silent_router_times_out() {
    let listener = TcpListener::bind((LOCALHOST, 0)).unwrap();
    let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
        panic!("bound an ipv4 address");
    };
    let (hold, held) = mpsc::channel::<()>();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = read_request(&mut stream);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nsome");
        let _ = held.recv();
    });
    let start = Instant::now();
    let got = external_ip(&ip_service(addr));
    let took = start.elapsed();
    drop(hold);
    assert!(
        matches!(got, Err(UpnpError::Timeout(a)) if a == addr),
        "{got:?}"
    );
    assert!(took >= Duration::from_millis(1900), "{took:?}");
    assert!(took < Duration::from_millis(3500), "{took:?}");
}

#[test]
fn endless_answer_is_cut_off() {
    let (addr, _rx) = fake_router(1, |_| {
        let mut answer = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        answer.resize(answer.len() + MAX_ANSWER + 4096, b'x');
        answer
    });
    let location = url(&format!("http://{addr}/rootDesc.xml"), LOCALHOST);
    assert!(matches!(describe(&location), Err(UpnpError::TooLarge)));
}

#[test]
fn search_takes_only_the_gateway_answer() {
    let good = "http://127.0.0.1:5000/rootDesc.xml";
    let (to, rx) = fake_ssdp(vec![
        ssdp(good).replace("InternetGatewayDevice:1\r\n", "MediaRenderer:1\r\n"),
        ssdp("http://127.0.0.2:5000/rootDesc.xml"),
        ssdp(good),
    ]);
    let mut notes = Vec::new();
    let location = search(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_secs(2),
        &mut |line| notes.push(line.to_string()),
    )
    .unwrap();
    assert_eq!(location, url(good, LOCALHOST));
    let ignored = notes.iter().filter(|n| n.starts_with("ignored")).count();
    assert_eq!(ignored, 2, "{notes:?}");

    let searches = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(searches.len(), 2);
    for (search, version) in searches.iter().zip(["1", "2"]) {
        assert!(search.starts_with("M-SEARCH * HTTP/1.1\r\n"), "{search}");
        assert!(search.contains(&format!("\r\nHOST: {to}\r\n")));
        assert!(search.contains("\r\nMAN: \"ssdp:discover\"\r\n"));
        assert!(search.contains("\r\nMX: 1\r\n"));
        assert!(search.contains(&format!(
            "\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:{version}\r\n"
        )));
        assert!(search.ends_with("\r\n\r\n"));
    }
}

#[test]
fn search_ignores_answers_from_another_address() {
    let (to, _rx) = fake_ssdp(vec![ssdp("http://127.0.0.2:5000/rootDesc.xml")]);
    let gateway = Ipv4Addr::new(127, 0, 0, 2);
    let mut notes = Vec::new();
    let got = search(
        gateway,
        LOCALHOST,
        to,
        Duration::from_millis(300),
        &mut |line| notes.push(line.to_string()),
    );
    assert!(
        matches!(got, Err(UpnpError::NoAnswer(g)) if g == gateway),
        "{got:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("did not come from the gateway")),
        "{notes:?}"
    );
}

#[test]
fn search_gives_up_after_the_wait() {
    let (to, _rx) = fake_ssdp(Vec::new());
    let start = Instant::now();
    let got = search(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_millis(300),
        &mut |_| {},
    );
    let took = start.elapsed();
    assert!(matches!(got, Err(UpnpError::NoAnswer(_))), "{got:?}");
    assert!(took >= Duration::from_millis(290), "{took:?}");
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn answer_flood_is_noted_briefly() {
    let mut answers = vec![ssdp("http://203.0.113.9/rootDesc.xml"); 20];
    answers.push(ssdp("http://127.0.0.1:5000/rootDesc.xml"));
    let (to, _rx) = fake_ssdp(answers);
    let mut notes = Vec::new();
    search(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_secs(2),
        &mut |line| notes.push(line.to_string()),
    )
    .unwrap();
    let noted = notes
        .iter()
        .filter(|n| n.starts_with("ignored an ssdp answer"))
        .count();
    assert_eq!(noted, 8, "{notes:?}");
    assert!(
        notes.contains(&String::from("ignored 12 more ssdp answers")),
        "{notes:?}"
    );
}

#[test]
fn find_takes_the_service_that_has_an_address() {
    let description = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device><deviceList><device><serviceList>
<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/ppp</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ip</controlURL></service>
</serviceList></device></deviceList></device></root>"#;
    let (http_addr, rx) = fake_router(3, move |request| {
        match request.lines().next().unwrap_or_default() {
            "GET /rootDesc.xml HTTP/1.1" => http("200 OK", description),
            "POST /ppp HTTP/1.1" => http(
                "200 OK",
                &soap_ok(
                    "GetExternalIPAddress",
                    "<NewExternalIPAddress></NewExternalIPAddress>",
                ),
            ),
            _ => http(
                "200 OK",
                &soap_ok(
                    "GetExternalIPAddress",
                    "<NewExternalIPAddress>203.0.113.7</NewExternalIPAddress>",
                ),
            ),
        }
    });
    let (to, _ssdp_rx) = fake_ssdp(vec![ssdp(&format!("http://{http_addr}/rootDesc.xml"))]);
    let mut notes = Vec::new();
    let router = find(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_secs(2),
        &mut |line| notes.push(line.to_string()),
    )
    .unwrap();
    assert_eq!(router.service.kind, Kind::WanIp1);
    assert_eq!(router.service.control.path(), "/ip");
    assert_eq!(router.external_ip, Ipv4Addr::new(203, 0, 113, 7));
    assert_eq!(requests(&rx).len(), 3);
    assert!(
        notes.iter().any(|n| n.starts_with("WANPPPConnection:1 at")),
        "{notes:?}"
    );
}

// The first answer the search hands out.
fn search(
    gateway: Ipv4Addr,
    local: Ipv4Addr,
    to: SocketAddr,
    wait: Duration,
    note: &mut dyn FnMut(std::fmt::Arguments<'_>),
) -> Result<Url, UpnpError> {
    Search::start(gateway, local, to, wait, note)?
        .next(note)?
        .ok_or(UpnpError::NoAnswer(gateway))
}

// An SSDP responder on loopback that sleeps through the first `lost`
// searches, as if they never arrived, and answers the next one.
fn fake_ssdp_after(lost: usize, answers: Vec<String>) -> SocketAddr {
    let socket = UdpSocket::bind((LOCALHOST, 0)).unwrap();
    let addr = socket.local_addr().unwrap();
    thread::spawn(move || {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 2048];
        let mut seen = 0;
        while let Ok((_, from)) = socket.recv_from(&mut buf) {
            seen += 1;
            if seen > lost {
                for answer in &answers {
                    let _ = socket.send_to(answer.as_bytes(), from);
                }
                return;
            }
        }
    });
    addr
}

#[test]
fn lost_search_is_sent_again() {
    let good = "http://127.0.0.1:5000/rootDesc.xml";
    // Both searches of the first pair lost.
    let to = fake_ssdp_after(2, vec![ssdp(good)]);
    let mut notes = Vec::new();
    let start = Instant::now();
    let location = search(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_secs(2),
        &mut |line| notes.push(line.to_string()),
    )
    .unwrap();
    assert_eq!(location, url(good, LOCALHOST));
    assert!(start.elapsed() >= Duration::from_millis(290));
    assert!(
        notes.contains(&String::from("sent the ssdp search again")),
        "{notes:?}"
    );
}

// Anyone on the LAN can answer the search in the router's name, and faster.
// A bad first answer costs one description, not UPnP for the whole room.
#[test]
fn find_goes_on_after_a_dead_answer() {
    let (dead, _dead_rx) = fake_router(1, |_| {
        // Past the search's wait, so the good answer is read afterwards.
        thread::sleep(Duration::from_millis(400));
        http("404 Not Found", "<html>no</html>")
    });
    let description = one_service("/ctl/IPConn");
    let (good, good_rx) = fake_router(2, move |request| {
        if request.starts_with("GET ") {
            http("200 OK", &description)
        } else {
            http(
                "200 OK",
                &soap_ok(
                    "GetExternalIPAddress",
                    "<NewExternalIPAddress>203.0.113.7</NewExternalIPAddress>",
                ),
            )
        }
    });
    let (to, _ssdp_rx) = fake_ssdp(vec![
        ssdp(&format!("http://{dead}/x.xml")),
        ssdp(&format!("http://{good}/rootDesc.xml")),
    ]);
    let mut notes = Vec::new();
    let router = find(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_millis(200),
        &mut |line| notes.push(line.to_string()),
    )
    .unwrap();
    assert_eq!(router.external_ip, Ipv4Addr::new(203, 0, 113, 7));
    assert_eq!(router.service.control.addr(), good);
    assert_eq!(requests(&good_rx).len(), 2);
    assert!(
        notes.iter().any(|n| n.starts_with(&format!(
            "the description at http://{dead}/x.xml led nowhere"
        ))),
        "{notes:?}"
    );
}

#[test]
fn at_most_four_descriptions_are_tried() {
    let (web, rx) = fake_router(8, |_| http("404 Not Found", "<html>no</html>"));
    let answers = ["a", "a", "b", "c", "d", "e", "f"]
        .iter()
        .map(|path| ssdp(&format!("http://{web}/{path}.xml")))
        .collect();
    let (to, _ssdp_rx) = fake_ssdp(answers);
    let got = find(
        LOCALHOST,
        LOCALHOST,
        to,
        Duration::from_millis(500),
        &mut |_| {},
    );
    assert!(matches!(got, Err(UpnpError::Status(404))), "{got:?}");
    let fetched: Vec<String> = requests(&rx)
        .iter()
        .map(|r| r.lines().next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        fetched,
        [
            "GET /a.xml HTTP/1.1",
            "GET /b.xml HTTP/1.1",
            "GET /c.xml HTTP/1.1",
            "GET /d.xml HTTP/1.1"
        ]
    );
}

fn xml_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "<",
        ">",
        "/",
        "</",
        "/>",
        "=",
        "\"",
        "'",
        " ",
        "\n",
        "?",
        "#",
        ":",
        "&",
        ";",
        "service",
        "serviceType",
        "controlURL",
        "URLBase",
        "root",
        "s:",
        "<![CDATA[",
        "]]>",
        "<!--",
        "-->",
        "<?",
        "?>",
        "<!",
        "&amp;",
        "&#x2F;",
        "&#0;",
        "\u{e9}",
        "<service>",
        "</service>",
        "<serviceType>",
        "</serviceType>",
        "<controlURL>",
        "</controlURL>",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
        "/ipc",
        "ipc",
        "http://192.168.0.1:5000/x",
        "http://192.168.0.2/x",
        "//192.168.0.1/y",
        "//10.0.0.1/y",
        "https://192.168.0.1/",
        "http://192.168.0.1@10.0.0.1/",
    ])
}

fn only_on_the_gateway(found: &[Service]) -> bool {
    found.iter().all(|service| {
        *service.control.addr().ip() == HOME_GATEWAY
            && service.control.path().starts_with('/')
            && service.control.path().bytes().all(|b| b.is_ascii_graphic())
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1500)) {
        let _ = ssdp_answer(&bytes, from_gateway(), HOME_GATEWAY);
        let _ = http_response(&bytes, false);
        let _ = http_response(&bytes, true);
        for action in [Action::GetExternalIpAddress, Action::AddPortMapping, Action::GetSpecificPortMappingEntry, Action::DeletePortMapping] {
            let _ = soap_answer(action, 200, &bytes);
            let _ = soap_answer(action, 500, &bytes);
        }
        if let Ok(found) = services(&bytes, &url("http://192.168.0.1:5000/desc.xml", HOME_GATEWAY)) {
            prop_assert!(only_on_the_gateway(&found));
        }
    }

    // Random markup reaches the tag scanner far more often than random bytes.
    #[test]
    fn random_markup_never_leads_off_the_gateway(pieces in proptest::collection::vec(xml_piece(), 0..120)) {
        let xml = pieces.concat();
        if let Ok(found) = scan(&xml) {
            prop_assert!(only_on_the_gateway(&found));
        }
        let _ = soap_answer(Action::AddPortMapping, 200, xml.as_bytes());
        let _ = soap_answer(Action::GetSpecificPortMappingEntry, 500, xml.as_bytes());
    }

    #[test]
    fn random_control_urls_never_lead_off_the_gateway(pieces in proptest::collection::vec(xml_piece(), 0..12)) {
        if let Ok(found) = scan(&one_service(&pieces.concat())) {
            prop_assert!(only_on_the_gateway(&found));
        }
    }

    #[test]
    fn damaged_descriptions_never_panic(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        let mut xml = PPP_STYLE.as_bytes().to_vec();
        for (at, byte) in edits {
            let at = at.index(xml.len());
            xml[at] = byte;
        }
        let gateway = Ipv4Addr::new(10, 0, 0, 138);
        if let Ok(found) = services(&xml, &url("http://10.0.0.138:5431/igd/desc.xml", gateway)) {
            prop_assert!(found.iter().all(|s| *s.control.addr().ip() == gateway));
        }
    }

    #[test]
    fn random_locations_stay_on_the_gateway(pieces in proptest::collection::vec(xml_piece(), 0..10)) {
        let location = pieces.concat();
        if let Ok(found) = ssdp_answer(ssdp(&location).as_bytes(), from_gateway(), HOME_GATEWAY) {
            prop_assert_eq!(*found.addr().ip(), HOME_GATEWAY);
            prop_assert!(found.path().bytes().all(|b| b.is_ascii_graphic()));
        }
    }

    #[test]
    fn chunked_bodies_come_back_whole(
        body in proptest::collection::vec(any::<u8>(), 0..3000),
        sizes in proptest::collection::vec(1usize..700, 1..10),
    ) {
        let mut answer = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let mut rest = body.as_slice();
        let mut size = sizes.iter().cycle();
        while !rest.is_empty() {
            let take = (*size.next().unwrap()).min(rest.len());
            answer.extend_from_slice(format!("{take:X}\r\n").as_bytes());
            answer.extend_from_slice(&rest[..take]);
            answer.extend_from_slice(b"\r\n");
            rest = &rest[take..];
        }
        answer.extend_from_slice(b"0\r\n\r\n");
        let got = http_response(&answer, false).unwrap().unwrap();
        prop_assert_eq!(got.body, body);
        prop_assert_eq!(http_response(&answer[..answer.len() - 1], false).unwrap(), None);
    }
}
