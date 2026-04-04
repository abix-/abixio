# AbixIO Project State

## Last Session: 2026-04-04 (infra)

### What We Worked On
Deployed Ascender (open-source AWX/Ansible automation platform) v25.3.5 on the k3s cluster in WSL2.

### Decisions Made
- Used official ascender-install Ansible playbooks from github.com/ctrliq/ascender-install
- Set kube_install: false (reused existing k3s cluster)
- HTTP only (no TLS -- sandbox use)
- Skipped Ledger component to stay within 8GB RAM budget
- Hostname: ascender.local, pointed /etc/hosts to ClusterIP (10.43.251.194) inside WSL2
- NodePort 30080 for access (AWX operator may revert to ClusterIP on reconciliation)

### Current State
- Ascender v25.3.5 running in `ascender` namespace (5 pods: operator, web, task, postgres, migration)
- Demo playbooks/templates pre-configured (CIS benchmarks, STIG, Fuzzball)
- Access: http://172.30.229.214:30080 from Windows (WSL2 NAT IP)
- Login: admin / ascender2024
- Installer source at ~/ascender-install inside WSL2
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
- Create an Ingress if Traefik is re-enabled on k3s
- NodePort may need re-patching after AWX operator reconciliation
