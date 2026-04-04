# AbixIO Project State

## Last Session: 2026-04-04 (infra)

### What We Worked On
Deployed Ascender (open-source AWX/Ansible automation platform) v25.3.5 on the k3s cluster in WSL2. Diagnosed and fixed WSL2 auto-shutdown causing repeated pod sandbox crashes.

### Decisions Made
- Used official ascender-install Ansible playbooks from github.com/ctrliq/ascender-install
- Set kube_install: false (reused existing k3s cluster)
- HTTP only (no TLS -- sandbox use)
- Skipped Ledger component to stay within 8GB RAM budget
- Hostname: ascender.local, /etc/hosts pointed to ClusterIP inside WSL2
- NodePort for all k3s services (no ingress controller -- traefik disabled)
- NodePort chosen over ingress for performance: fewer hops, no proxy bottleneck for future storage services
- AWX CR `service_type: NodePort` is the right way to expose (not patching the service directly -- operator reconciles)
- Fixed WSL2 vmIdleTimeout=-1 in .wslconfig to prevent auto-shutdown killing k3s
- Will use awx-ui with Ascender

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |

### Current State
- Ascender v25.3.5 running in `ascender` namespace (5 pods: operator, web, task, postgres, migration)
- Demo playbooks/templates pre-configured (CIS benchmarks, STIG, Fuzzball)
- Access: http://<WSL2_IP>:30080 from Windows (check IP: `wsl -d Ubuntu-24.04 -- bash -c "hostname -I | awk '{print \$1}'"`)
- Login: admin / ascender2024
- Installer source at ~/ascender-install inside WSL2
- WSL2 stable after vmIdleTimeout fix
- AbixIO code unchanged -- no source modifications this session

### AbixIO State (unchanged from prior session)
- **Progress: ~95%**
- **101 tests** (88 unit + 13 integration), 0 clippy warnings
- Healing fully wired: MRF auto-enqueue, scanner loop, heal_object(), graceful shutdown

### Next Steps (AbixIO)
- Multipart upload (needed for files >8MB with AWS CLI)
- Object versioning
- Bucket replication
- Live smoke test of healing

### Next Steps (Ascender)
- Consider TLS setup if keeping long-term
- Add Ledger if more RAM becomes available
