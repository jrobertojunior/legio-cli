# Pairing protocol, version 1

How a phone pairs with a machine that runs `legio`. The phone makes its
key, and only the public half leaves the phone.

## 1. The QR code

`legio` prints a QR code that holds one JSON object:

```json
{
  "v": 1,
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
| `v` | Always `1`. The app refuses any other version. |
| `name` | The machine's hostname. Use it as the default connection name. |
| `host`, `port`, `user` | The SSH login. |
| `targetPort` | The bridge port on 127.0.0.1. |
| `session` | The Herdr session the app attaches panes from. |
| `hostKey` | The SHA-256 fingerprint of the server's **ed25519** host key, in `ssh-keygen -l` form. Optional: it is left out when the server has no ed25519 host key. When it is present, the app must refuse an SSH server whose host key does not match. When it is absent, the app saves the key it meets on the first login and refuses a different key after that. |
| `pairPort` | The TCP port on `host` that takes the public key. |
| `token` | A one-time secret: 32 random bytes, base64url without padding. |

The code holds no login. The token is good for one pairing, for 10 minutes
by default.

## 2. The request

The phone makes an ed25519 key pair and keeps the private key in the
Keychain. Then it sends:

```
POST http://<host>:<pairPort>/v1/pair
Content-Type: application/json

{
  "publicKey": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI… anything",
  "device": "Jose's iPhone",
  "mac": "<base64>",
  "push": { "connectionId": "<the app's connection id>", "sealed": "<base64>" }
}
```

- `push` is optional. See section 2a.
- `publicKey` is the OpenSSH public key line. Only `ssh-ed25519` is
  accepted. The comment is ignored.
- `device` is a name for the phone. The server changes each character
  other than `A–Z a–z 0–9 . _ -` to `-`, and keeps 40 characters at most.
- `mac` is standard base64 (with padding) of
  `HMAC-SHA256(key: UTF-8 bytes of token, message)`, where `message` is the
  UTF-8 bytes of:

  ```
  "herdr-pair-v1\n" + publicKey + "\n" + device
  ```

  Use the `token` string as it is. Do not decode it.

In Swift:

```swift
let message = "herdr-pair-v1\n\(publicKey)\n\(device)"
let mac = HMAC<SHA256>.authenticationCode(
    for: Data(message.utf8),
    using: SymmetricKey(data: Data(token.utf8))
)
let macBase64 = Data(mac).base64EncodedString()
```

The server holds the request open while a person at the terminal compares
key phrases and answers. Set the request timeout to at least 3 minutes.
While the phone waits, it must show the key phrase of its own public key
(section 4), so the person can compare it with the one the terminal shows.

## 2a. The push secret

When the phone allows notifications, it sends its relay device secret in
`push`, so the machine can send it notifications at once. The secret is a
secret, and the request is plain HTTP, so it is sealed:

- `key` = `HMAC-SHA256(key: UTF-8 bytes of token, message: "legio-push-v1")`,
  32 bytes.
- `sealed` = standard base64 of ChaCha20-Poly1305 with that key: a random
  12-byte nonce, then the ciphertext of the secret's UTF-8 bytes, then the
  16-byte tag. This is CryptoKit's `ChaChaPoly.SealedBox.combined`.
- The additional authenticated data is the UTF-8 bytes of
  `"legio-push-v1\n" + connectionId`, so a sealed secret cannot be moved
  to another connection id.
- `connectionId` is the app's id for this connection: 1 to 100 characters
  of `A–Z a–z 0–9 - _`. Each notification carries it, so a tap opens this
  machine.

The machine opens the seal only after it added the key. A seal that does
not open costs the notifications, never the pairing. A machine that does
not know `push` ignores it.

## 3. The reply

The body is always JSON: `{"status": …, "message": …, "phrase": …}`.
`phrase` is the key phrase of the key that was sent. It is present only on
`accepted`, `declined` and `failed`.

| HTTP | `status` | Meaning | Listener after |
| --- | --- | --- | --- |
| 200 | `accepted` | The key is in `authorized_keys`. Save the connection. | Closed |
| 403 | `declined` | The person said no. | Closed |
| 401 | `bad_token` | The MAC is wrong. | Open, until 5 bad requests |
| 400 | `bad_request`, `bad_key` | The body or the key cannot be read. `message` says why. | Open |
| 500 | `failed` | The server could not write the key. | Closed |
| 404, 405 | `not_found`, `bad_request` | Wrong path or method. | Open |

A closed listener spent the token. To pair again, run `legio pair` on
the server for a new code.

## 4. The key phrase

The person compares five words, not a fingerprint. Both sides work them
out the same way:

1. Take the public key blob: the bytes that the base64 in the second field
   of the `ssh-ed25519` line decodes to.
2. Take its SHA-256 digest.
3. Read the first 55 bits of the digest, big-endian, as five numbers of 11
   bits. Each number is an index into the word list.
4. Join the five words with `-`, in lowercase: `fee-jazz-naive-fruit-equip`.

The word list is the BIP-39 English list (2048 words, so a word carries 11
bits and five words carry 55). It is `src/wordlist.txt` in this repository,
and its SHA-256 is
`2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda`. The
app carries the same list in `KeyPhrase.swift`. If the two lists differ, the
phone and the terminal show different words for the same key.

A test vector: the key
`AAAAC3NzaC1lZDI1NTE5AAAAIHI2iP/D59jopcwuQ7odefdufWyYlto1QwkLRcmzaf87`
has the phrase `fee-jazz-naive-fruit-equip`.

55 bits: to pass a key of their own off as the phone's, someone must find
one whose digest starts with the same 55 bits. That is about 3.6 × 10^16
tries, inside the time that the pairing code lives. The host key
fingerprint in the QR code stays in the `SHA256:` form, because it is
compared with what an SSH client shows.

## 5. After pairing

The phone logs in as `user` with its private key. The entry in
`authorized_keys` looks like this:

```
restrict,pty,port-forwarding,permitopen="127.0.0.1:4499" ssh-ed25519 AAAA… legio-app:Jose-s-iPhone
```

The key can open a PTY and forward to the bridge port, plus the ports that
`--forward-port` added. It can do nothing else.

## Why it is safe on plain HTTP

- A public key is not a secret, so no one learns anything from reading it
  on the network.
- A key that someone else sends does not carry a valid MAC unless they
  have the token, and the token is only in the QR code.
- If someone else did see the QR code, the person at the terminal sees a
  device name and a key phrase that do not match their phone, and says
  no. A no spends the token.
- The reply is not signed. A forged `accepted` only makes the phone save a
  connection whose login then fails. It does not open anything.
