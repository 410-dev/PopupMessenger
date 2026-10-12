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
| `unknown_tenancy` | `hello`: 이 서버에 없는 Tenancy ID (8절) |
| `unsupported` | 다른 서버의 주소: 정책은 허용하지만 서버 간 전송을 아직 지원하지 않음 (9.3) |

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
→ { "t": "hello", "rid": "1", "proto": 1, "app": "0.2.0", "tenancy": "example" }
← { "t": "hello", "rid": "1", "proto": 1,
    "server": { "id": "<서버 고유 ID>", "name": "사내 릴레이", "version": "0.1.0", "domain": "relay.example.com" },
    "tenancy": { "id": "example", "name": "Example Inc." },
    "features": ["device_target", "hold", "tenancy", "address", "departments"],
    "policy": { … } }
```

`features`는 같은 프로토콜 버전 안에서 나중에 더해진 기능입니다(없으면 그 기능이 없는 서버).
`device_target`: send의 `to`에 기기 하나를 지정할 수 있음(4.6). `hold`: 릴레이 수신 설정(4.10).
`tenancy`: `hello`에 Tenancy를 지정할 수 있음(8절). `address`: send의 `to`와 `resolve`에 주소를 쓸 수 있음(9절).
`departments`: `departments` 요청(9.2).

`tenancy`는 접속할 Tenancy ID입니다. 빼면 Unmigrated Tenancy로 접속합니다(8.3). 없는 ID면 `hello` 대신
`unknown_tenancy` 오류가 옵니다. 정책(`policy`)은 그 Tenancy의 정책입니다. 전송 연결의 `hello`에는 `tenancy`를
넣지 않아도 됩니다(세션 토큰이 Tenancy를 알려 줌).

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
    "account": "<계정 ID>", "username": "hong",
    "tenancy": { "id": "example", "name": "Example Inc.", "moved": false },
    "address": "hong:<기기 ID>@example:relay.example.com" }
```

- `tenancy`는 실제로 로그온한 Tenancy입니다. Unmigrated Tenancy로 접속했는데 계정이 다른 Tenancy로 옮겨졌다면
  `moved: true`와 옮겨 간 Tenancy가 오고, 바로 뒤에 그 Tenancy의 `event: policy`가 옵니다(8.3). 클라이언트는 저장한
  Tenancy ID를 이 값으로 바꿉니다.
- `address`는 이 기기의 주소입니다(9절). 사용자 이름이 없는 계정은 계정 ID를 씁니다.

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
      { "account": "9f…", "username": "kim", "name": "김철수", "departments": ["dev", "ops"],
        "status": "online", "online": true, "last_seen": 1767099990000, "removed": false,
        "devices": [
          { "id": "a1b2…", "name": "김철수", "host": "", "status": "online", "online": true,
            "notify": true, "max_files": 10, "max_file_size": 10737418240,
            "pubkey": "<base64>", "last_seen": 1767099990000, "hold": true } ],
        "updated": 1767099990000 } ] }
```

- 기기의 `hold`: 그 기기가 오프라인일 때 서버가 쪽지를 보관해 주는지(4.10). 꺼져 있고 오프라인이면 보낼 수 없습니다.
- `departments`: 그 계정이 속한 부서 ID(9.2). 연락처는 같은 Tenancy 안의 계정만 보여 줍니다.
- ID가 `api-`로 시작하는 기기는 API 키(10절)입니다. 이름이 `API: …`이고 늘 오프라인으로 보입니다.
- `updated`: 항목이 바뀔 때마다 커지는 값. `contacts` 응답과 `event: contact`가 엇갈려 도착하면 클라이언트는 `updated`가 더 큰 쪽을 남깁니다.

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
    "to": ["<계정 ID>", "<계정 ID>/<기기 ID>", …],
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

- `to`에는 주소(9절)도 쓸 수 있습니다. `@`가 들어 있으면 주소입니다. 주소 하나가 여러 계정에 닿으면(와일드카드)
  `result`에 계정마다 한 줄씩, 같은 `to`로 들어갑니다. `result`와 `rejected`의 각 줄에는 `account`가, 다른 Tenancy의
  계정이면 `tenancy`도 붙습니다. 와일드카드 주소에 보내는 사람 자신이 들어가면 자신은 빠집니다.
- 서버는 첨부 개수와 각 크기를 `relay_max_files`, `relay_max_file_size`, 그리고 받는 사람 기기 각각의 `max_files`, `max_file_size`와 비교합니다.
  다른 Tenancy로 보낼 때는 받는 Tenancy의 정책(첨부 한도, 본문 한도, 종단간 암호화 필수 여부, 저장 공간)도 함께 봅니다. 한도를 넘는 받는 사람은 `rejected`에 넣습니다. 받을 수 있는 사람이 하나도 없으면 `ready` 대신 `err`를 보냅니다.
- `"<계정 ID>/<기기 ID>"`(`device_target`)는 그 기기에만 보냅니다. 결과의 `to`는 요청에 쓴 문자열 그대로입니다. 내 계정의 다른 기기에도 이렇게 보낼 수 있습니다(나 자신과 내 계정 전체는 안 됨).
- 오프라인이면서 `hold`가 꺼진 기기는 빠집니다. 남는 기기가 없으면 `code: "offline"`으로 거부합니다.
- 계정만 쓰면 그 계정에 등록된 **모든 기기**에 각각 전달 항목을 만듭니다. 온라인인 기기에는 바로 `event: inbox`를 보냅니다. `delivered`는 기기가 하나 이상 알림을 받았다는 뜻이고, `stored`는 모든 기기가 오프라인이라는 뜻입니다. 둘 다 서버에 보관되며, 기기마다 `ack`할 때까지 남아 있습니다.
- 여러 사람에게 보내도 파일은 한 번만 올리고, 서버도 한 벌만 저장합니다.
- 계정 단위로 보내면 보낸 사람 계정의 다른 기기에는 보내지 않습니다.

### 4.7 inbox / fetch / ack

```json
← { "t": "event", "e": "inbox", "count": 2 }              // 제어 연결로 오는 알림

→ { "t": "inbox", "rid": "6" }                             // 제어 연결
← { "t": "inbox", "rid": "6",
    "list": [ { "mid": "…", "from": "<계정 ID>", "from_device": "<기기 ID>", "from_name": "김철수",
                "from_address": "kim:<기기 ID>@example:relay.example.com",
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

- `from_address`: 보낸 기기의 주소(9절). `fetch` 응답에도 있습니다. 다른 Tenancy에서 온 쪽지는 보낸 사람이 연락처에
  없으므로, 클라이언트는 이 주소로 보낸 사람을 표시하고 답장합니다. Tenancy 도입 전에 보관된 쪽지는 빈 문자열입니다.
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

### 4.10 hold — 릴레이 수신 (`hold` 기능)

```json
→ { "t": "hold", "rid": "10" }                          // 읽기
→ { "t": "hold", "rid": "10", "mode": "device_off" }    // 바꾸기
← { "t": "hold", "rid": "10", "mode": "allow" | "device_off" | "all_off" }
```

오프라인인 동안 서버가 쪽지를 보관해 줄지 정합니다. `allow`: 보관(기본). `device_off`: 이 기기는 보관하지 않음.
`all_off`: 이 계정의 어느 기기도 보관하지 않음(계정 설정이라 다른 기기에서도 보임). `allow`로 바꾸면 계정 설정과 이 기기 설정을
모두 켭니다. 바꾸면 연락처가 갱신되고(`event: contact`) 감사 기록에 `relay_receive`가 남습니다. 이미 보관된 쪽지는 그대로입니다.

---

## 5. 클라이언트 경로 선택

쪽지를 보낼 때 받는 사람마다 다음 순서로 경로를 고릅니다.

받는 사람은 **기기**입니다. LAN의 피어 ID와 릴레이의 기기 ID가 같으므로, LAN과 릴레이 양쪽에 보이는 기기도
목록에서 한 줄입니다. "릴레이 계정이 있다"는 그 기기가 같은 릴레이(`server.id`)에 등록되어 있다는 뜻으로,
연락처 목록이나 LAN 탐색 메타의 `relay` 필드(6장)로 압니다. 릴레이로 보낼 때는 그 기기만 지정합니다
(`"<계정>/<기기>"`, 이 기능이 없는 서버면 계정 전체).

| 순서 | 조건 | 경로 |
|---|---|---|
| 1 | 릴레이 계정이 있고 `force_relay` | 릴레이 |
| 2 | `force_relay`와 `block_direct_outsiders`가 모두 켜져 있고, 받는 사람이 이 릴레이를 쓰지 않음 | 실패: "서버 정책상 보낼 수 없음" |
| 3 | LAN에서 온라인 (최근 95초 안에 탐색 응답) | LAN 직접 (정책 미적용) |
| 4 | 3이 안 됐고(오프라인·LAN 실패) 릴레이 계정이 있음 | 릴레이 (온라인이면 바로 알림, 아니면 `hold`가 켜진 경우 보관 후 전달) |
| 5 | 그 밖 | 실패: "오프라인" |

목록의 상태 표시(색 원):

| 표시 | 뜻 |
|---|---|
| 파란색 꽉 찬 원 | LAN과 릴레이 모두 온라인 |
| 파란색 도넛 | LAN에만 온라인 |
| 초록색 꽉 찬 원 | 릴레이로만 온라인 |
| 초록색 도넛 | 오프라인이지만 릴레이가 보관했다가 전달 (`hold` 켜짐) |
| 빨간색 도넛 | 온라인, 방해금지 |
| 회색 도넛 | 오프라인이고 릴레이로도 보낼 수 없음 |

- 0.1.8 이전의 `r:<계정 ID>` 받는 사람(그 계정의 기기 전체)은 예전 쪽지에 답장할 때만 쓰입니다. 그룹에 있던 것은 그 계정의 기기들로 바뀝니다.
- 릴레이로 받은 쪽지의 보낸 사람은 보낸 기기(`from_device`)이므로, 답장도 그 기기로 갑니다(LAN 우선).
- 릴레이 폴백은 네트워크 실패(연결 실패·시간 초과·전송 중 끊김·완료 응답 없음)에만 씁니다. 받는 쪽이 거부한 경우(첨부 개수·크기 한도, 거절 응답)는 그대로 실패입니다.
- 마지막으로 본 곳이 LAN인 기기도, 오프라인이면 릴레이 계정이 있는 한 릴레이로 보냅니다.
- LAN 탐색으로 처음 알게 된 계정은 `lookup`(계정 ID)으로 연락처에 연결해 기기 목록과 공개키를 받습니다.
- 보낸 쪽지 기록에는 받는 사람마다 경로를 남깁니다: `LAN` / `릴레이` / `릴레이 보관 (만료 시각)`.
- 그룹 쪽지도 받는 사람마다 따로 경로를 고릅니다. 오프라인인 그룹원도 4번 조건이 맞으면 받는 사람에 넣습니다.
- `block_direct_outsiders`는 클라이언트가 지키는 규칙입니다. 서버는 LAN 직접 전송을 볼 수 없기 때문입니다.

## 6. LAN 탐색 확장 (Meta)

LAN 피어끼리 같은 릴레이를 쓰는지 알 수 있도록, 탐색 메타에 선택 필드를 추가합니다. 이 필드가 없는 이전 버전 피어와도 호환됩니다.

```json
"relay": { "server": "<server.id>", "tenancy": "<Tenancy ID>", "account": "<계정 ID>" }
```

클라이언트가 여러 릴레이에 연결해 있으면 그중 하나(목록의 첫 연결)를 싣습니다. `tenancy`가 없으면 `unmigrated`로 봅니다.
"같은 릴레이"는 `server`와 `tenancy`가 모두 같다는 뜻입니다.

## 7. 서버 저장과 만료

- 맡긴 쪽지: `mid`, 보낸 기기와 계정, 받는 기기별 전달 상태, `body`, 파일 블롭, 생성 시각, 본문 만료 시각, 파일 만료 시각.
- 서버는 1분마다 만료 처리를 합니다.
  - 파일 만료 시각이 지나면 블롭을 지우고 `files_expired`로 표시합니다.
  - 본문 만료 시각이 지나면 아직 `ack`하지 않은 기기마다 만료 알림(보낸 계정별 개수)을 쌓고 쪽지를 지웁니다.
  - 모든 기기가 `ack`한 쪽지는 바로 지웁니다.
- 연락처: 계정(ID, username, 이름, 상태), 기기(ID, 프로필, 공개키, 기기 비밀값 해시, 마지막 접속 시각과 IP, 앱 버전).

---

## 8. Tenancy

릴레이 서버 하나에 여러 조직(Tenancy)을 둘 수 있습니다. 서버 제공 업체가 서버 A에는 회사 A·B·C를, 서버 B에는
회사 D·E·F를 두는 식입니다.

### 8.1 격리

- Tenancy마다 데이터베이스, 첨부 저장소, 사용자, 기기, 연락처, 정책(2절), 감사 로그가 따로 있습니다. 연락처·`lookup`·
  계정 ID로 보내기는 같은 Tenancy 안에서만 됩니다. 다른 Tenancy로는 주소(9절)로만, 정책이 허용할 때만 보낼 수 있습니다.
- 같은 기기(같은 `Settings.id`)가 한 서버의 여러 Tenancy에 각각 등록될 수 있습니다. Tenancy마다 계정과 기기 비밀값이 따로입니다.
- 정책은 Tenancy 관리자가 정합니다. 물리 자원(Tenancy 저장 공간, 파일 크기 상한, 본문 상한, 사용자 수, 아카이브 첨부 보관)과
  서버 운영 설정(TLS, WebSocket, 프록시, 서버 전체 저장 공간, 서버 주소)은 서버 총괄 관리자만 정합니다.
  Tenancy 정책의 `relay_max_file_size`는 Tenancy의 파일 크기 상한을 넘지 않습니다.

### 8.2 클라이언트의 연결

- 연결 하나는 (서버 주소, Tenancy ID) 쌍입니다. 클라이언트는 연결을 여러 개 둘 수 있고(같은 서버의 여러 Tenancy, 여러 서버),
  연결마다 제어 연결, 기기 비밀값, 연락처, 받은 쪽지 경로가 따로입니다. 같은 릴레이인지는 (`server.id`, `tenancy.id`)로 판단합니다.
- 새 연결을 추가할 때 Tenancy ID는 필수입니다(빈 값은 받지 않음).
- 구형 클라이언트, 그리고 Tenancy 도입 전에 저장된 연결은 `hello`에 `tenancy`를 넣지 않습니다. 이 연결은 Unmigrated Tenancy로 갑니다.

### 8.3 Unmigrated Tenancy와 리디렉션

- 서버를 Tenancy 지원 버전으로 올리면 기존 데이터(계정, 기기, 보관 쪽지, 설정, 감사 로그) 전체가 `unmigrated` Tenancy가 되고,
  기존 관리자는 서버 총괄 관리자가 됩니다. `unmigrated`는 지울 수 없습니다.
- 총괄 관리자는 Unmigrated의 사용자를 다른 Tenancy로 옮길 수 있습니다. 계정 ID, 기기 ID, 기기 비밀값이 그대로 가고,
  그 기기들을 기다리던 쪽지와 첨부도 함께 갑니다. 옮긴 계정은 Unmigrated에서 `removed`로 보입니다.
- 옮긴 뒤 그 기기가 `tenancy` 없이(또는 `unmigrated`로) 로그온하면, 서버는 기기 ID(새 기기면 사용자 이름)로 옮겨 간 Tenancy를 찾아
  그곳으로 로그온시킵니다. `login` 응답의 `tenancy.moved`가 `true`이고, 그 Tenancy의 `event: policy`가 이어집니다.
  구형 클라이언트는 아무것도 모른 채 계속 쓰고, 신형 클라이언트는 저장된 Tenancy ID를 바꿉니다.

---

## 9. 주소와 부서

### 9.1 주소

```
<id>[:<device>]@[<departments>:]<tenancy>:<server>
john:948asfsdddfgjsa@developers:example:relay.example.com
```

| 부분 | 뜻 | 생략 | `*` |
|---|---|---|---|
| `id` | 사용자 이름. 사용자 이름이 없는 계정은 계정 ID | 불가 | 그 Tenancy(또는 부서)의 모든 사용자 |
| `device` | 기기 ID | 생략하면 모든 기기 | 모든 기기 |
| `departments` | 부서 ID. 여러 개는 `,`로 구분(`dev,ops`). 그중 하나에 속한 사람만 | 생략하면 부서를 따지지 않음 | 모든 부서 |
| `tenancy` | Tenancy ID | 불가 | 불가 |
| `server` | 서버 주소(`hello`의 `server.domain`). 포트를 붙일 수 있음(`relay.example.com:45443`) | 불가 | 불가 |

- 사용자 이름에는 `@`가 들어갈 수 있으므로 마지막 `@`를 기준으로 나눕니다.
- 서버 주소는 관리자가 정한 `domain`, 없으면 `public_url`의 호스트, 없으면 인증서 이름입니다. 서버는 이 이름들을 모두 자기 자신으로 봅니다.
- 주소 하나가 닿는 계정은 1000개까지입니다.
- 장치를 지정했는데 그 사용자가 그 기기를 갖고 있지 않으면 그 사용자는 빠집니다(`*:<device>@…`는 그 기기의 주인 한 명).

### 9.2 부서

Tenancy 관리자가 부서를 만들고 사용자를 넣습니다. 사용자는 부서를 여러 개 가질 수 있습니다. 부서 ID는 1~32자의 영문 소문자,
숫자, `.`, `_`, `-`입니다.

```json
→ { "t": "departments", "rid": "11" }
← { "t": "departments", "rid": "11", "list": [ { "id": "dev", "name": "개발팀", "members": 12 } ] }
```

연락처 항목의 `departments`로 부서별 사람을 알 수 있습니다. 클라이언트는 연락처 검색에서 부서를 골라 그 부서원 전체를 받는 사람으로
넣을 수 있습니다(주소 `*@<부서>:<tenancy>:<server>`로 보내도 같음).

### 9.3 resolve

```json
→ { "t": "resolve", "rid": "12", "address": "*@dev:beta:relay.example.com" }
← { "t": "resolve", "rid": "12", "address": "…", "tenancy": "beta",
    "list": [ { …4.4의 연락처 항목…, "tenancy": "beta", "address": "kim@beta:relay.example.com" } ] }
```

주소가 닿는 계정과 기기(공개키 포함)를 알려 줍니다. 다른 Tenancy의 사람에게 E2EE로 보낼 때 이것으로 키를 받습니다.
보내기와 같은 정책 검사를 거치며, 허용되지 않으면 `policy`, 없는 사람이면 `unknown_user`, 다른 서버면 `policy` 또는 `unsupported` 오류입니다.
기기를 지정한 주소면 `devices`에 그 기기만 남습니다.

### 9.4 Tenancy 사이의 교환 정책

다른 Tenancy로 쪽지와 첨부파일을 보내려면 서버와 양쪽 Tenancy가 모두 허용해야 합니다.

서버(총괄 관리자):

| 설정 | 뜻 | 기본 |
|---|---|---|
| `exchange.cross_tenancy` | 이 서버의 Tenancy끼리 주고받기 | 꺼짐 |
| `exchange.send_remote` | 다른 서버로 보내기 | 꺼짐 |
| `exchange.receive_remote` | 다른 서버에서 받기 | 꺼짐 |
| Tenancy별 예외 (`limits.cross_tenancy` / `send_remote` / `receive_remote`) | 비워 두면 서버 설정을 따르고, 켜거나 끄면 그 Tenancy만 예외 | 없음 |

Tenancy(Tenancy 관리자, `exchange`):

| 설정 | 뜻 |
|---|---|
| `send_local` / `receive_local` | 같은 서버의 다른 Tenancy로 보내기 / 받기 |
| `send_remote` / `receive_remote` | 다른 서버의 Tenancy로 보내기 / 받기 |
| `allow` | `tenancy:server` 목록. 비어 있지 않으면 이 목록에 있는 곳만. `*:server`는 그 서버의 모든 Tenancy, 서버를 뺀 `tenancy`는 이 서버 |
| `deny` | 같은 형식. 여기 있으면 무조건 막음(`allow`보다 우선) |

A Tenancy에서 B Tenancy(같은 서버)로 보내기 = 서버가 A·B 모두에 `cross_tenancy`를 허용 ∧ A의 `send_local` ∧ A의 목록이 B를 허용 ∧
B의 `receive_local` ∧ B의 목록이 A를 허용.

다른 서버로 보내기는 서버·Tenancy의 `send_remote`와 목록을 검사한 뒤, 서버 간 전송이 아직 없으므로 `unsupported`로 거부합니다.

받는 Tenancy에는 쪽지 사본이 그 Tenancy의 보관 기간으로 저장되고, 받는 Tenancy의 아카이브가 켜져 있으면 그쪽에도 남습니다.
보낸 쪽 Tenancy의 아카이브에는 보낸 쪽지 전체가 남습니다.

---

## 10. API 키 (스크립트용 HTTP API)

터미널의 간단한 스크립트가 쪽지를 보내거나 받을 수 있도록, 사용자 계정에 묶인 API 키를 발급합니다.

- Tenancy 정책 `api_keys`가 켜져 있어야 합니다. 종단간 암호화(`e2ee`) Tenancy에서는 쓸 수 없습니다(스크립트는 평문을 주고받음).
- 사용자는 사용자 포털에서, Tenancy 관리자는 관리 화면에서 계정마다 키를 발급하고 폐기합니다. 키는 발급할 때 한 번만 보여 주고
  서버는 해시만 저장합니다. 형식: `pm.<tenancy>.<키 ID>.<비밀값>`.
- 권한은 `send`(보내기)와 `receive`(받기)입니다. 받기 키는 계정에 `api-<키 ID>` 기기를 하나 더 만듭니다. 계정 앞으로 온 쪽지는
  이 기기 몫으로도 보관되어, 스크립트가 `ack`할 때까지(또는 보관 기간까지) 남습니다. 이 기기는 기기 수 한도에 들지 않습니다.
- 정책이 꺼지거나 계정이 정지되면 키도 막힙니다. 틀린 키가 계속 오면 잠깁니다(로그인 잠금 설정을 따름).

릴레이 포트에서 HTTP(S)로 호출합니다(WebSocket과 같은 포트). 사용자 포털을 꺼도 동작합니다.

```
Authorization: Bearer pm.example.0a1b2c3d4e5f6789.xxxxxxxx
```

| 요청 | 설명 |
|---|---|
| `GET /api/v1/me` | 키 정보: `tenancy`, `account`, `username`, `address`, `send`, `receive` |
| `POST /api/v1/send` | `{ "to": ["kim", "<계정 ID>", "*@dev:example:relay.example.com"], "text": "…", "files": [ { "name": "a.txt", "data": "<base64>" } ] }`. `@`가 없는 받는 사람은 이 Tenancy의 사용자 이름 또는 계정 ID. 응답은 `{ "mid", "result" }`(4.6의 `sent`와 같음). 요청 본문은 64 MiB까지 |
| `GET /api/v1/inbox` | 이 키가 받을 쪽지 목록(최대 500개). 항목마다 `mid`, `from`, `from_name`, `from_address`, `ts`, `text`, `group`, `files[{idx,size,name}]`, `files_expired` |
| `GET /api/v1/messages/{mid}` | 쪽지 하나(형식은 위와 같음) |
| `GET /api/v1/messages/{mid}/files/{idx}` | 첨부파일 원본 |
| `POST /api/v1/messages/{mid}/ack` | 받기 완료. 이후 목록에서 빠짐 |

오류는 `{ "error": "<사람이 읽을 메시지>", "code": "<1절의 code>" }`이고, HTTP 상태는 401(키가 틀림), 403(권한·정책), 404(없음),
507(저장 공간 부족), 400(그 밖)입니다.

```sh
curl -s https://relay.example.com/api/v1/send -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' -d '{"to":["kim"],"text":"빌드 끝"}'
```
