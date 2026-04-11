# Encryption (decision pending)

No encryption feature is currently implemented in abixio. This document tracks
options and threat analysis for future work only.

## Threat Model

| Attacker Access | Realistic? | What They Get |
|---|---|---|
| Stolen disk (powered off) | Yes | Raw shard files |
| Shell on server (no root) | Yes | Can read disk files as abixio user |
| Root on server | Possible | Process memory, env vars, all disk files |
| Root on server + root on client | Unlikely but worst case | Everything. No crypto scheme survives this |

## Options Under Consideration

### Option A: No App Encryption + OS Full-Disk Encryption

- AbixIO stores plaintext shards
- BitLocker (Windows) / LUKS (Linux) on disk volumes
- **Protects:** stolen/powered-off disks
- **Fails:** any running-system compromise (FDE is transparent while mounted)
- **Complexity:** zero in AbixIO
- **Debugging:** easy. Shards are readable with `cat`

### Option B: SSE-S3 (Server-Side, Master Key from Env Var)

- Per-object DEK (data encryption key), AES-256-GCM
- DEK encrypted with master key, stored in meta.json
- Master key from `ABIXIO_MASTER_KEY` env var
- **Protects:** disk theft, shell access that can read files but not process memory
- **Fails:** root reads `/proc/<pid>/environ` or process memory
- **Complexity:** modest Rust implementation using standard AEAD crates plus
  metadata changes
- **Performance:** negligible (~3-5 GB/s AES-GCM with AES-NI)

### Option C: Client-Side Encryption (CSE)

- Encrypt on client before upload, AbixIO stores opaque ciphertext
- Server never sees keys or plaintext
- Tool: `age` (modern, simple) or `gpg`
- **Protects:** full server compromise including root. Server only has ciphertext
- **Fails:** root on client machine (captures plaintext in memory after decrypt)
- **Complexity:** zero in AbixIO, wrapper scripts on client
- **UX cost:** can't just `aws s3 cp`. You need encrypt/decrypt wrappers

### Option D: CSE + TPM-Sealed Key

- Same as Option C, but the age private key is sealed to the laptop's TPM chip
- Key bound to specific hardware + boot state (PCR values) + PIN
- **Protects:** server compromise, offline laptop theft, boot chain tampering (evil maid)
- **Fails:** root on running client (reads plaintext from RAM after TPM unseals and age decrypts)
- **Complexity:** zero in AbixIO, TPM setup on client (`tpm2-tools` or Windows CNG API)
- **Cost:** $0 (TPM already in laptop)

### Option E: CSE + YubiKey

- age private key lives on YubiKey hardware, non-extractable
- `age-plugin-yubikey` handles decrypt operations on-device
- **Protects:** server compromise, remote client compromise (key not on disk), laptop theft without YubiKey
- **Fails:** root on running client while YubiKey is plugged in (captures age output in memory)
- **Complexity:** zero in AbixIO, `age-plugin-yubikey` on client
- **Cost:** ~$50

## Comparison Matrix

| | Disk theft | Server shell | Server root | Client root | Both root | UX friction | AbixIO code |
|---|---|---|---|---|---|---|---|
| **A: FDE only** | Safe | Exposed | Exposed | N/A | Exposed | None | None |
| **B: SSE-S3** | Safe | Safe | Exposed | N/A | Exposed | None | ~250 LOC |
| **C: CSE (age)** | Safe | Safe | Safe | Exposed | Exposed | Wrappers | None |
| **D: CSE + TPM** | Safe | Safe | Safe | Exposed* | Exposed | Wrappers + TPM setup | None |
| **E: CSE + YubiKey** | Safe | Safe | Safe | Exposed* | Exposed | Wrappers + YubiKey | None |

*D and E: root on client must catch plaintext in-flight (harder, but possible). Key itself stays protected.

## Key Insight

No encryption scheme survives "root on the machine doing the decryption." The plaintext must exist in memory at some point. Layers reduce the blast radius of partial compromise; they don't prevent total compromise.

The real security investment is **not getting rooted**: patching, minimal attack surface, firewalls, monitoring, SSH key-only access, not running services as root.

## Leaning Toward

TBD. Likely some combination of:
- FDE on server disks (baseline, free)
- CSE with age on client (server never sees keys)
- TPM or YubiKey for key storage on client (key never on disk)

No AbixIO code changes needed for any of these. Encryption stays external.

## Current Status

- No SSE-S3 implementation
- No SSE-KMS implementation
- No SSE-C implementation
- No at-rest shard encryption in abixio metadata or storage paths
- Operators should use OS-level full-disk encryption or external client-side
  encryption if encryption is required today
