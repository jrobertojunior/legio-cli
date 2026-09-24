# Pairing protocol, version 2

How a phone pairs with a machine that runs `herdr-setup`. Version 1 was
`vps-setup.sh`: it made the key on the server and put the private half in
the QR code. In version 2 the phone makes its key, and only the public half
leaves the phone.

## 1. The QR code

`herdr-setup` prints a QR code that holds one JSON object:

```json
{
  "v": 2,
  "name": "vps-01",
  "host": "100.64.0.3",
  "port": 22,
  "user": "deploy",
  "targetPort": 4499,
  "session": "default",
  "hostKey": "SHA256:VV6Cxeygzx+7nZSxKmffDDZQ3vRqEcuxu5zIFfTIwP0",
  "pairPort": 7450,
  "token": "6E513nahocTDApWwh1VKRBn-_B6iY8F1btZVbCw-eRI"
}
```

| Field | Meaning |
| --- | --- |
| `v` | Always `2`. Version 1 has a `key` field and no `token`. |
| `name` | The machine's hostname. Use it as the default connection name. |
| `host`, `port`, `user` | The SSH login. |
| `targetPort` | The bridge port on 127.0.0.1, as in version 1. |
| `session` | The Herdr session, as in version 1. |
| `hostKey` | The SHA-256 fingerprint of the server's **ed25519** host key, in `ssh-keygen -l` form. Optional: it is left out when the server has no ed25519 host key. When it is present, the app must refuse an SSH server whose host key does not match. |
| `pairPort` | The TCP port on `host` that takes the public key. |
| `token` | A one-time secret: 32 random bytes, base64url without padding. |

The code holds no login. The token is good for one pairing, for 10 minutes
by default.

## 2. The request

The phone makes an ed25519 key pair and keeps the private key in the
Keychain. Then it sends:

```
POST http://<host>:<pairPort>/v2/pair
Content-Type: application/json

{
  "publicKey": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI… anything",
  "device": "Jose's iPhone",
  "mac": "<base64>"
}
```

- `publicKey` is the OpenSSH public key line. Only `ssh-ed25519` is
  accepted. The comment is ignored.
- `device` is a name for the phone. The server changes each character
  other than `A–Z a–z 0–9 . _ -` to `-`, and keeps 40 characters at most.
- `mac` is standard base64 (with padding) of
  `HMAC-SHA256(key: UTF-8 bytes of token, message)`, where `message` is the
  UTF-8 bytes of:

  ```
  "herdr-pair-v2\n" + publicKey + "\n" + device
  ```

  Use the `token` string as it is. Do not decode it.

In Swift:

```swift
let message = "herdr-pair-v2\n\(publicKey)\n\(device)"
let mac = HMAC<SHA256>.authenticationCode(
    for: Data(message.utf8),
    using: SymmetricKey(data: Data(token.utf8))
)
let macBase64 = Data(mac).base64EncodedString()
```

The server holds the request open while a person at the terminal compares
fingerprints and answers. Set the request timeout to at least 3 minutes.
While the phone waits, it must show the SHA-256 fingerprint of its own
public key, so the person can compare it with the one the terminal shows.

## 3. The reply

The body is always JSON: `{"status": …, "message": …, "fingerprint": …}`.
`fingerprint` is present only on `accepted`, `declined` and `failed`.

| HTTP | `status` | Meaning | Listener after |
| --- | --- | --- | --- |
| 200 | `accepted` | The key is in `authorized_keys`. Save the connection. | Closed |
| 403 | `declined` | The person said no. | Closed |
| 401 | `bad_token` | The MAC is wrong. | Open, until 5 bad requests |
| 400 | `bad_request`, `bad_key` | The body or the key cannot be read. `message` says why. | Open |
| 500 | `failed` | The server could not write the key. | Closed |
| 404, 405 | `not_found`, `bad_request` | Wrong path or method. | Open |

A closed listener spent the token. To pair again, run `herdr-setup pair` on
the server for a new code.

## 4. After pairing

The phone logs in as `user` with its private key. The entry in
`authorized_keys` looks like this:

```
restrict,pty,port-forwarding,permitopen="127.0.0.1:4499" ssh-ed25519 AAAA… herdr-app:Jose-s-iPhone
```

The key can open a PTY and forward to the bridge port, plus the ports that
`--forward-port` added. It can do nothing else.

## Why it is safe on plain HTTP

- A public key is not a secret, so no one learns anything from reading it
  on the network.
- A key that someone else sends does not carry a valid MAC unless they
  have the token, and the token is only in the QR code.
- If someone else did see the QR code, the person at the terminal sees a
  device name and a fingerprint that do not match their phone, and says
  no. A no spends the token.
- The reply is not signed. A forged `accepted` only makes the phone save a
  connection whose login then fails. It does not open anything.
