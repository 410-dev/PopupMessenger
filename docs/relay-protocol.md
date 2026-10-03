# 릴레이 서버 프로토콜 v1

PopupMessenger의 선택적 셀프호스팅 릴레이 서버와 클라이언트 사이의 프로토콜입니다. 서버 구현은 [`relay/`](../relay)에 있습니다.

릴레이는 다음 세 가지를 맡습니다.

1. **외부망 중계**: LAN 브로드캐스트가 닿지 않는 사용자끼리 서버를 거쳐 쪽지를 주고받습니다.
2. **보관 후 전달**(store-and-forward): 받는 사람이 오프라인이어도 서버에 맡겨 두었다가, 받는 사람의 기기가 로그온하면 자동으로 받아 팝업으로 띄웁니다.
3. **연락처**: 서버가 사용자 목록을 갖고 있다가, 로그온한 클라이언트가 요청하면 내려줍니다.

전송 한도, 암호화 수준, 가입 방식 같은 규칙은 서버 **정책**(2절)이 정합니다. 정책은 릴레이를 거치는 전송에만 적용되며, 예외는 `block_direct_outsiders` 하나입니다(5절).

---

## 1. 전송 계층

| 항목 | 값 |
|---|---|
| 연결 | TCP. 기본 포트 `45443`. 또는 같은 포트의 WebSocket (아래) |
| 프레이밍 | LAN 프로토콜과 같음: `u32 big-endian 길이` + `UTF-8 JSON`. 프레임 최대 8 MiB |
| 바이너리 | 파일 본문은 JSON 프레임 다음에 원본 바이트를 그대로 이어 보냄 |
| TLS | 서버는 연결의 첫 바이트로 구분함. `0x16`(TLS ClientHello)이면 TLS 1.3, `G`이면 HTTP(WebSocket 업그레이드), 그 밖은 평문 프레임 |
| 인증서 | 운영자가 넣은 인증서를 쓰고, 없으면 서버가 처음 실행될 때 자체 서명 인증서를 만듦. 클라이언트는 OS 신뢰 저장소로 검증하고, 실패하면 처음 연결할 때 SHA-256 지문을 사용자에게 보여 준 뒤 그 지문을 고정함(TOFU) |

**WebSocket 전송**: HTTPS만 통과시키는 프록시(Cloudflare Tunnel 등)를 위해, 위와 똑같은 바이트 스트림을 WebSocket 바이너리 메시지에 나눠 실어 보낼 수 있습니다. 메시지 경계는 의미가 없고, 받는 쪽은 메시지를 이어 붙여 같은 프레임 스트림으로 읽습니다.

- 클라이언트는 주소가 `https://`/`wss://`이면 WSS(기본 포트 443), `http://`/`ws://`이면 WS(기본 포트 80)로 연결하고, 그 밖의 주소는 TCP로 연결합니다. 경로를 붙일 수 있습니다(`https://host/relay`).
- 서버는 `hello` 응답에 `"transport": { "kind": "tcp|tls|ws|wss|ws-proxy", "ws_chunk": <바이트> }`를 넣습니다. 양쪽 모두 메시지 하나를 `ws_chunk` 이하로 보냅니다(관리자 설정, 기본 256 KiB). 첨부파일은 여러 메시지로 스트리밍되므로, HTTP 요청 본문 크기 제한(Cloudflare 100 MB 등)과 관계가 없습니다.
- 프록시가 TLS를 처리하면 서버에는 평문 WebSocket으로 들어옵니다. 관리자가 "프록시가 TLS 처리"를 켜면, `X-Forwarded-Proto: https` 또는 `CF-Visitor`에 https가 있는 WebSocket 연결을 암호화된 연결(`ws-proxy`)로 인정하고, 접속자 IP는 `CF-Connecting-IP` / `X-Forwarded-For`에서 가져옵니다.

클라이언트는 TCP 주소에 대해서는 **항상 TLS로 먼저 연결**합니다. 정책이 `plain`인 서버도 TLS를 받아 줍니다. TLS 연결이 아예 안 되는 서버(평문 전용 프록시 뒤 등)에 한해서, 사용자가 서버를 처음 등록할 때만 평문으로 다시 시도합니다. 한 번 TLS로 연결된 서버에 평문으로 다시 붙지는 않습니다(다운그레이드 방지). 정책이 `tls` 또는 `e2ee`인데 평문으로 연결하면 서버는 `hello` 다음에 `tls_required` 오류를 보내고 연결을 끊습니다.

연결은 두 종류입니다.

- **제어 연결**: 로그온부터 로그오프까지 계속 유지합니다. 상태, 연락처, 새 쪽지 알림이 이 연결로 오갑니다. 클라이언트는 30초마다 `ping`을 보내고, 서버는 90초 동안 아무 프레임도 받지 못하면 연결을 끊습니다.
- **전송 연결**: 쪽지 하나를 보내거나 받을 때마다 새로 엽니다. 파일 스트리밍이 제어 연결을 막지 않도록 분리한 것입니다. `hello` 다음 프레임은 `auth`이며, 제어 연결에서 받은 `session` 토큰으로 인증합니다.

모든 요청에는 클라이언트가 정한 `rid`(요청 ID, 문자열)를 붙이고, 응답에는 같은 `rid`를 돌려줍니다. 서버가 먼저 보내는 알림(`event`)에는 `rid`가 없습니다.

오류 응답 형식은 하나입니다.

```json
{ "t": "err", "rid": "…", "code": "auth_required", "reason": "사람이 읽을 메시지" }
```

| code | 의미 |
|---|---|
| `bad_request` | 형식 오류 |
| `version` | 지원하지 않는 프로토콜 버전 |
| `tls_required` | 정책상 TLS가 필요한데 평문으로 연결함 |
| `auth_required` | 이 서버는 사용자 이름과 비밀번호가 필요함 |
| `auth_failed` | 로그인 실패 (계정이 없거나 비밀번호·기기 비밀값이 틀림) |
| `locked` | 로그인 실패가 너무 많아 잠시 잠김. `retry_after`(초)가 붙음 |
| `registration_closed` | 가입이 닫힌 서버 |
| `username_taken` | `register`: 이미 쓰이는 사용자 이름 |
| `pending` | 가입 신청 완료, 관리자 승인 대기 중 |
| `disabled` | 관리자가 계정 또는 기기를 정지함 |
| `device_limit` | 계정의 기기 수 한도에 도달함 |
| `session` | 세션 만료 또는 무효. 다시 로그온해야 함 |
| `policy` | 정책 위반 (파일 수·크기 초과, E2EE 서버에 평문 본문을 보냄 등) |
| `keys` | E2EE: 받는 사람 기기 중 키가 빠진 기기가 있음. 연락처를 새로 받은 뒤 다시 보내야 함 |
| `unknown_user` | 받는 사람이 이 서버에 없음 |
| `quota` | 서버 또는 계정의 저장 공간이 부족함 |
| `not_found` | 보관함에 없는 쪽지 (만료 포함) |

---

## 2. 정책

서버는 `hello` 응답에 정책을 실어 보내고, 운영자가 정책을 바꾸면 `event: policy`로 다시 보냅니다. 클라이언트는 이 값에 맞춰 화면을 바꾸고(로그인 창 표시 여부, 첨부 한도 등), 서버는 요청을 받을 때마다 정책을 다시 검사합니다.

```json
"policy": {
  "auth": "password",
  "registration": "closed",
  "max_devices": 3,
  "relay_max_files": 10,
  "relay_max_file_size": 104857600,
  "force_relay": false,
  "block_direct_outsiders": false,
  "encryption": "tls",
  "retention_hours": { "text": 168, "files": 24 },
  "contacts": "all"
}
```

| 필드 | 값 | 의미 |
|---|---|---|
| `auth` | `"none"` \| `"password"` | `none`: 사용자 이름·비밀번호 없이 기기 ID와 기기 비밀값만으로 로그온하며, 계정 하나에 기기 하나. `password`: 계정 로그인 필수 |
| `registration` | `"closed"` \| `"approval"` \| `"open"` | 처음 보는 사용자의 가입 처리 방식. `closed`(기본): 관리자가 만든 계정만 사용. `approval`: 신청은 받고 관리자가 승인. `open`: 누구나 바로 가입 (봉사자가 운영하는 공개 서버용) |
| `max_devices` | 1 ~ 9999 | `password` 서버에서 계정 하나에 등록할 수 있는 기기 수. 관리자가 계정별로 따로 정할 수 있음. 9999가 고정 최대치 |
| `relay_max_files` | u32 (0이면 첨부 금지) | 릴레이를 거치는 쪽지 하나에 붙일 수 있는 첨부파일 수 |
| `relay_max_file_size` | u64 바이트 (0이면 첨부 금지) | 릴레이를 거치는 파일 하나의 최대 크기 |
| `force_relay` | bool | `true`면 이 서버의 연락처에 있는 사람에게는 LAN에서 보여도 항상 릴레이로 보냄 |
| `block_direct_outsiders` | bool | `force_relay`일 때만 의미가 있음. `true`면 이 서버를 쓰지 않는 LAN 사용자에게 직접 보내는 것도 금지함 (5절) |
| `encryption` | `"plain"` \| `"tls"` \| `"e2ee"` | 아래 표 참고 |
| `retention_hours.text` | 1 ~ 87600 시간 | 맡긴 쪽지(본문)를 지우기까지의 시간 |
| `retention_hours.files` | 1 ~ 87600 시간 | 맡긴 첨부파일을 지우기까지의 시간. 실제로는 `text`보다 길어지지 않음. 파일만 먼저 만료되면 본문만 전달하고 "보관 기간 만료"로 표시 |
| `contacts` | `"all"` \| `"lookup"` | `all`: 연락처에 모든 사용자가 보임. `lookup`: 주고받은 적이 있거나 직접 찾은(4.5) 사용자만 보임. 사용자가 많은 공개 서버용 |

암호화 단계:

| `encryption` | 통신 구간 | 서버 저장 | 서버가 볼 수 있는 것 |
|---|---|---|---|
| `plain` | 평문 TCP 허용 | 평문 | 전부 |
| `tls` | TLS 1.3 필수 | 평문 | 전부 (통신 구간 도청만 막음) |
| `e2ee` | TLS 1.3 필수 | 암호문 | 보낸 사람, 받는 사람, 크기, 시각. 본문·파일 이름·파일 내용·그룹 정보는 볼 수 없음 |

---

## 3. 신원

- **계정**(`account`): 연락처와 받는 사람의 단위입니다. 서버가 만드는 16자리 hex ID로 구분합니다. `password` 서버에서는 계정마다 `username`이 있습니다.
- **기기**(`device`): 클라이언트 설치 하나입니다. 기존 `Settings.id`(16자리 hex)를 기기 ID로 그대로 씁니다. LAN과 릴레이가 같은 ID를 쓰므로, LAN에서 보이는 피어와 릴레이 연락처를 같은 사람으로 묶을 수 있습니다.
- **기기 비밀값**(`device_secret`): 클라이언트가 처음 연동할 때 32바이트 난수를 만들어 서버마다 따로 보관합니다. 다른 사람이 같은 기기 ID로 로그온하는 것을 막는 데 쓰며, 서버는 SHA-256 해시만 저장합니다. 높은 엔트로피의 난수라서 느린 해시를 쓰지 않습니다. 비밀번호는 Argon2id로 저장합니다.
- **E2EE 키**: 기기마다 X25519 키 쌍을 만들고, 로그온할 때 공개키를 서버에 올립니다. 연락처에는 기기별 공개키가 들어 있습니다. 상대 공개키가 바뀌면 클라이언트는 쪽지 창에 "보안 키가 바뀌었습니다"라는 경고를 띄우고, 사용자가 확인하기 전까지 그 기기에 E2EE 전송을 보류합니다. 키 지문은 공개키 SHA-256의 앞 8바이트를 hex로 쓴 것입니다.
- `auth: none` 서버에서는 기기 하나가 곧 계정 하나입니다. 처음 로그온할 때 계정이 자동으로 만들어집니다(`registration` 정책에 따름).

---

## 4. 메시지

### 4.1 hello (모든 연결의 첫 프레임)

```json
→ { "t": "hello", "rid": "1", "proto": 1, "app": "0.2.0" }
← { "t": "hello", "rid": "1", "proto": 1,
    "server": { "id": "<서버 고유 ID>", "name": "사내 릴레이", "version": "0.1.0" },
    "policy": { … } }
```

`server.id`는 서버를 처음 설치할 때 만들어지는 무작위 ID입니다. 클라이언트는 이 값으로 "같은 릴레이에 연동된 적이 있는지"를 판단합니다(5절).

### 4.2 register / login (제어 연결)

```json
→ { "t": "register", "rid": "2",                          // password 서버의 가입
    "user": "hong", "pass": "…",
    "id": "<기기 ID>", "device_secret": "<base64>", "pubkey": "<base64>",
    "profile": { … } }

→ { "t": "login", "rid": "2",
    "user": "hong", "pass": "…",                           // auth: password 일 때만
    "id": "<기기 ID>", "device_secret": "<base64>",
    "pubkey": "<base64 X25519 공개키>",                     // encryption: e2ee 일 때 필수
    "profile": { "name": "홍길동", "host": "DEV-PC-01", "status": "online",
                 "notify": true, "max_files": 10, "max_file_size": 10737418240 } }

← { "t": "login", "rid": "2", "session": "<불투명 토큰>",
    "account": "<계정 ID>", "username": "hong" }
```

- `register`가 성공하면 `login`과 같은 응답을 보내고 바로 로그온 상태가 됩니다. `approval` 서버에서는 `pending` 오류를 보냅니다. 이후 관리자가 승인하면 같은 정보로 `login`할 수 있습니다.
- `auth: none` 서버에서는 `register`가 따로 없습니다. 처음 보는 기기가 `login`하면 `registration` 정책에 따라 자동 가입(`open`), 승인 대기(`approval` → `pending`), 거부(`closed` → `registration_closed`) 중 하나가 됩니다.
- `password` 서버에서 **이미 등록된 기기는 `user`/`pass` 없이 기기 비밀값만으로 로그온**합니다. 클라이언트는 비밀번호를 저장하지 않고, 비밀번호는 기기를 계정에 처음 추가할 때만 씁니다. 기기를 막으려면 관리자가 기기를 정지하거나 등록 해제합니다.
- `user`/`pass`를 함께 보내면 계정 비밀번호와 기기 비밀값을 둘 다 확인합니다. 새 기기면 기기 수 한도를 확인하고(`device_limit`) 등록합니다. 다른 계정에 등록된 기기 ID로 로그인하면, 기기 비밀값이 맞을 때 그 기기를 새 계정으로 옮깁니다.
- 같은 기기가 이미 로그온해 있으면 이전 제어 연결에 `event: kicked`를 보내고 끊습니다.
- 계정 또는 기기 단위로 연속해서 로그인에 실패하면 잠깁니다(`locked`). 횟수와 잠금 시간은 관리자 설정입니다.
- `session` 토큰은 전송 연결 인증에 씁니다. 제어 연결이 끊기면 무효가 됩니다.

### 4.3 profile

```json
→ { "t": "profile", "rid": "3", "profile": { "name": "홍길동", "status": "dnd", … } }
← { "t": "ok", "rid": "3" }
```

이름, 상태, 알림 설정, 첨부 한도가 바뀔 때마다 보냅니다. 서버는 이 계정을 볼 수 있는 다른 온라인 기기에 `event: contact`로 알립니다.

### 4.4 contacts

```json
→ { "t": "contacts", "rid": "4", "since": 1767000000000 }
← { "t": "contacts", "rid": "4", "now": 1767100000000, "full": false,
    "list": [
      { "account": "9f…", "username": "kim", "name": "김철수",
        "status": "online", "online": true, "last_seen": 1767099990000, "removed": false,
        "devices": [
          { "id": "a1b2…", "name": "김철수", "host": "", "status": "online", "online": true,
            "notify": true, "max_files": 10, "max_file_size": 10737418240,
            "pubkey": "<base64>", "last_seen": 1767099990000 } ] } ] }
```

- `since`를 생략하거나 0이면 `full: true`로 전체 목록을 보냅니다. 값이 있으면 그 시각 이후에 바뀐 계정만 보냅니다. 삭제된 계정은 `removed: true`로 옵니다. 클라이언트는 응답의 `now`를 다음 `since`로 씁니다.
- 계정의 `status`는 그 계정 기기들의 상태 가운데 가장 활발한 것입니다(online > dnd > offline). `online`은 로그온해 있고 상태가 offline이 아닌 기기가 하나라도 있다는 뜻입니다.
- `host`는 운영자가 공개를 허용했을 때만 채워집니다.
- 자기 계정도 목록에 포함됩니다(내 다른 기기를 알 수 있도록).
- 클라이언트는 로그온 직후와 사용자가 "다시 탐색"을 누를 때 이 요청을 보냅니다. 그 사이의 변경은 `event: contact`로 받습니다.

### 4.5 lookup (`contacts: lookup` 서버)

```json
→ { "t": "lookup", "rid": "5", "query": "kim" }          // username 또는 계정 ID와 정확히 일치
← { "t": "lookup", "rid": "5", "contact": { …4.4의 항목… } }   // 없으면 not_found
```

찾은 계정은 양쪽 연락처에 서로 추가됩니다. 쪽지를 주고받아도 서로 추가됩니다. `contacts: all` 서버에서도 동작합니다.

### 4.6 send (전송 연결)

```json
→ { "t": "auth", "rid": "1", "session": "<토큰>" }
← { "t": "ok", "rid": "1" }
→ { "t": "send", "rid": "2", "msg": "<클라이언트 쪽지 ID>",
    "to": ["<계정 ID>", …],
    "files": [ { "size": 1048576 } ],
    "body": { … } }                     // 4.8 참고
← { "t": "ready", "rid": "2", "mid": "<서버 쪽지 ID>",
    "rejected": [ { "to": "…", "code": "policy", "reason": "…" } ] }
   (파일 원본 바이트를 순서대로 스트리밍)
← { "t": "sent", "rid": "2", "mid": "…",
    "result": [ { "to": "<계정 ID>", "state": "delivered", "devices": 2, "online": 1 },
                { "to": "…", "state": "stored", "devices": 1, "online": 0, "expires": 1767700000000 },
                { "to": "…", "state": "failed", "code": "policy", "reason": "…" } ] }
```

- 서버는 첨부 개수와 각 크기를 `relay_max_files`, `relay_max_file_size`, 그리고 받는 사람 기기 각각의 `max_files`, `max_file_size`와 비교합니다. 한도를 넘는 받는 사람은 `rejected`에 넣습니다. 받을 수 있는 사람이 하나도 없으면 `ready` 대신 `err`를 보냅니다.
- 받는 사람 계정에 등록된 **모든 기기**에 각각 전달 항목을 만듭니다. 온라인인 기기에는 바로 `event: inbox`를 보냅니다. `delivered`는 기기가 하나 이상 알림을 받았다는 뜻이고, `stored`는 모든 기기가 오프라인이라는 뜻입니다. 둘 다 서버에 보관되며, 기기마다 `ack`할 때까지 남아 있습니다.
- 여러 사람에게 보내도 파일은 한 번만 올리고, 서버도 한 벌만 저장합니다.
- 보낸 사람 계정의 다른 기기에는 보내지 않습니다.

### 4.7 inbox / fetch / ack

```json
← { "t": "event", "e": "inbox", "count": 2 }              // 제어 연결로 오는 알림

→ { "t": "inbox", "rid": "6" }                             // 제어 연결
← { "t": "inbox", "rid": "6",
    "list": [ { "mid": "…", "from": "<계정 ID>", "from_device": "<기기 ID>", "from_name": "김철수",
                "ts": 1767100000000, "files": [ { "size": 1048576 } ], "files_expired": false } ],
    "expired": [ { "from": "<계정 ID>", "from_name": "김철수", "count": 3, "last": 1767000000000 } ] }

→ { "t": "expired_ack", "rid": "7" }                       // expired 목록을 사용자에게 보여 준 뒤
← { "t": "ok", "rid": "7" }

→ { "t": "fetch", "rid": "2", "mid": "…" }                 // 전송 연결 (auth 다음)
← { "t": "fetch", "rid": "2", "mid": "…", "from": "<계정 ID>", "from_device": "…", "from_name": "…",
    "ts": …, "files": [ { "size": 1048576 } ], "files_expired": false, "body": { … } }
   (파일 원본 바이트 스트리밍)
→ { "t": "ack", "rid": "3", "mid": "…" }                   // 다 받아 저장한 뒤
← { "t": "ok", "rid": "3" }
```

- 클라이언트는 로그온 직후와 `event: inbox`를 받을 때마다 `inbox` → `fetch` → `ack`를 차례로 실행합니다. 받은 쪽지는 LAN 수신과 같은 경로로 쪽지함에 넣고 팝업을 띄웁니다. 방해금지 상태면 쪽지함에만 넣습니다.
- `ack`는 **그 기기 몫**만 완료 처리합니다. 받는 사람 계정의 기기가 여러 대면, 모든 기기가 `ack`해야 전달이 끝난 것으로 봅니다. 그 전까지는 계속 보관하고, 보관 기간이 지나면 남은 기기가 있어도 지웁니다.
- `ack` 전에 연결이 끊기면 다음 로그온 때 다시 받습니다. 클라이언트는 `mid`로 중복을 거릅니다.
- 첨부 보관 기간이 지나면 `files_expired: true`, `files: []`로 본문만 전달합니다. 클라이언트는 쪽지에 "첨부파일 보관 기간이 지나 삭제되었습니다"라는 메모를 붙입니다.
- **만료 알림**: 본문 보관 기간이 지나 쪽지를 지울 때, 서버는 아직 받지 못한 기기마다 "보낸 사람별로 만료된 쪽지 개수"만 남깁니다. 이 정보는 `inbox` 응답의 `expired`로 전달되고, `expired_ack`를 받으면 지웁니다. 클라이언트는 "김철수 님이 보낸 쪽지 3개가 보관 기간이 지나 삭제되었습니다"처럼 표시합니다.

### 4.8 body

`encryption`이 `plain` 또는 `tls`일 때:

```json
"body": { "enc": "none",
          "text": "본문",
          "names": ["보고서.pdf"],
          "group": { "name": "개발팀", "members": [ { "id": "…", "name": "…" } ] } }
```

`group`은 LAN 프로토콜 v3와 같은 형식이고, 보낸 사람이 공개를 선택했을 때만 들어갑니다.

`encryption`이 `e2ee`이면 서버는 `enc: "none"`인 본문을 `policy` 오류로 거부합니다.

```json
"body": { "enc": "x25519-xchacha20poly1305-v1",
          "keys": { "<받는 사람 기기 ID>": "<base64 포장된 콘텐츠 키>", … },
          "msg": "<클라이언트 쪽지 ID>",
          "nonce": "<base64 24바이트>",
          "data": "<base64 암호문: {text, names, group} JSON>" }
```

- 서버는 받는 사람 계정의 모든 기기에 대한 `keys`가 들어 있는지 확인합니다. 빠진 기기가 있으면 그 받는 사람을 `keys` 오류로 거부합니다. `fetch` 응답의 `body.keys`에는 받는 기기 자신의 키만 남깁니다.
- 쪽지마다 무작위 32바이트 콘텐츠 키 `K`를 만듭니다.
- `data` = XChaCha20-Poly1305(K, nonce, JSON{text, names, group}). AAD는 `body.msg`(클라이언트 쪽지 ID)입니다. 서버가 `body`를 그대로 돌려주므로 받는 쪽도 같은 값을 씁니다.
- 기기별 `keys[id]` = `crypto_box`(K, 받는 기기 공개키, **보낸 기기 비밀키**). X25519 + XSalsa20-Poly1305를 쓰고, 24바이트 nonce를 앞에 붙입니다. 보낸 기기의 정적 키로 포장하므로, 받는 쪽은 연락처에 있는 보낸 기기의 공개키로 풀면서 보낸 사람이 맞는지도 확인하게 됩니다.
- 파일 본문은 K에서 HKDF-SHA256(info = `"file" ‖ 파일 순번`)으로 파일 키를 만들어, 64 KiB 청크마다 XChaCha20-Poly1305 STREAM 방식(청크 카운터 + 마지막 청크 표시)으로 암호화합니다. 청크마다 16바이트 태그가 붙으므로 `files[].size`는 **암호문 크기**이고, 한도 검사도 암호문 크기로 합니다.
- 파일 이름은 `data` 안의 `names`에만 들어 있습니다.

### 4.9 기타

```json
→ { "t": "ping", "rid": "8" }                   ← { "t": "pong", "rid": "8" }
→ { "t": "logout", "rid": "9" }                 ← { "t": "ok", "rid": "9" }   // 앱 종료 (LAN의 bye)
← { "t": "event", "e": "contact", "contact": { …4.4의 항목… } }
← { "t": "event", "e": "policy", "policy": { … } }
← { "t": "event", "e": "kicked", "reason": "다른 곳에서 같은 기기로 로그온했습니다" }
```

---

## 5. 클라이언트 경로 선택

쪽지를 보낼 때 받는 사람마다 다음 순서로 경로를 고릅니다.

| 순서 | 조건 | 경로 |
|---|---|---|
| 1 | 받는 사람이 내 릴레이 연락처에 있고 `force_relay` | 릴레이 |
| 2 | `force_relay`와 `block_direct_outsiders`가 모두 켜져 있고, 받는 사람이 이 릴레이를 쓰지 않음 | 실패: "서버 정책상 보낼 수 없음" |
| 3 | LAN에서 온라인 (최근 95초 안에 탐색 응답) | LAN 직접 (정책 미적용) |
| 4 | 받는 사람과 나 모두 **같은 릴레이**(`server.id`)의 연락처에 있음 | 릴레이 (온라인이면 바로 알림, 아니면 보관 후 전달) |
| 5 | 그 밖 | 실패: "오프라인" |

- 3번 LAN 직접 전송이 연결 단계에서 실패하면, 4번 조건이 맞을 때 릴레이로 다시 시도합니다.
- 보낸 쪽지 기록에는 받는 사람마다 경로를 남깁니다: `LAN` / `릴레이` / `릴레이 보관 (만료 시각)`.
- 그룹 쪽지도 받는 사람마다 따로 경로를 고릅니다. 오프라인인 그룹원도 4번 조건이 맞으면 받는 사람에 넣습니다.
- `block_direct_outsiders`는 클라이언트가 지키는 규칙입니다. 서버는 LAN 직접 전송을 볼 수 없기 때문입니다.

## 6. LAN 탐색 확장 (Meta)

LAN 피어끼리 같은 릴레이를 쓰는지 알 수 있도록, 탐색 메타에 선택 필드를 추가합니다. 이 필드가 없는 이전 버전 피어와도 호환됩니다.

```json
"relay": { "server": "<server.id>", "account": "<계정 ID>" }
```

## 7. 서버 저장과 만료

- 맡긴 쪽지: `mid`, 보낸 기기와 계정, 받는 기기별 전달 상태, `body`, 파일 블롭, 생성 시각, 본문 만료 시각, 파일 만료 시각.
- 서버는 1분마다 만료 처리를 합니다.
  - 파일 만료 시각이 지나면 블롭을 지우고 `files_expired`로 표시합니다.
  - 본문 만료 시각이 지나면 아직 `ack`하지 않은 기기마다 만료 알림(보낸 계정별 개수)을 쌓고 쪽지를 지웁니다.
  - 모든 기기가 `ack`한 쪽지는 바로 지웁니다.
- 연락처: 계정(ID, username, 이름, 상태), 기기(ID, 프로필, 공개키, 기기 비밀값 해시, 마지막 접속 시각과 IP, 앱 버전).
