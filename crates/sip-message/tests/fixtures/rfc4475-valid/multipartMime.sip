MESSAGE sip:kumiko@example.org SIP/2.0
Via: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-d87543-4dade06d0bdb11ee-1--d87543-;rport
Max-Forwards: 70
Route: <sip:127.0.0.1:5080>
Contact: <sip:fluffy@127.0.0.1:5070>
To: <sip:kumiko@example.org>
From: <sip:fluffy@example.com>;tag=2fb0dcc9
Call-ID: 3d9485ad0c49859b@Zmx1ZmZ5LW1hYy0xNi5sb2NhbA..
CSeq: 1 MESSAGE
Content-Type: multipart/mixed;boundary=7a9cbec02ceef655
Date: Sat, 15 Oct 2005 04:44:56 GMT
User-Agent: SIPimp.org/0.2.5 (curses)
Content-Length: 106

--7a9cbec02ceef655
Content-Type: text/plain
Content-Transfer-Encoding: binary

Hello
--7a9cbec02ceef655
Content-Type: text/plain

World
--7a9cbec02ceef655--
