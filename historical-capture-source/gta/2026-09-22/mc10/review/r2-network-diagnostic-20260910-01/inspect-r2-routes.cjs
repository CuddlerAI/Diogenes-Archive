// ROOT-only, read-only: uses the already configured local controller helper.
// Does not access R2 credentials, request uploads, change routes, or print other traffic.
const path = require('node:path');
const crypto = require('node:crypto');
const {api} = require('../../../mc2-20260908/vpn-routing.cjs');
const anon = s => crypto.createHash('sha256').update(String(s)).digest('hex').slice(0,12);
const isR2 = host => /(?:^|\.)r2\.cloudflarestorage\.com$/i.test(host || '');
const isRclone = value => /(?:^|[\\/])rclone(?:\.exe)?$/i.test(value || '');
const safeHost = host => isR2(host) ? '*.r2.cloudflarestorage.com' : '(rclone host redacted)';
function selectConnections(data) {
  return (data.connections || []).filter(c => isR2(c.metadata?.host) ||
    isRclone(c.metadata?.processPath) || isRclone(c.metadata?.process)).map(c => ({
      connection_id_hash:anon(c.id), host:safeHost(c.metadata?.host),
      process:isRclone(c.metadata?.processPath) || isRclone(c.metadata?.process) ? 'rclone.exe' : '(unresolved)',
      network:c.metadata?.network, inbound_type:c.metadata?.type,
      destination_port:c.metadata?.destinationPort,
      route:(c.chains || []).includes('DIRECT') ? 'DIRECT' : 'PROXY_OR_OTHER',
      chain_aliases:(c.chains || []).map(x => ['DIRECT','REJECT'].includes(x) ? x : 'policy-'+anon(x)),
      rule:c.rule, rule_payload:(/rclone|cloudflarestorage/i.test(c.rulePayload || '') ?
        String(c.rulePayload).replace(/[^.\s]+\.r2\.cloudflarestorage\.com/gi,'*.r2.cloudflarestorage.com') : '(unrelated payload redacted)'),
      upload_bytes:c.upload, download_bytes:c.download
    }));
}
async function main() {
  const configs = await api('GET','/configs');
  const first = selectConnections(await api('GET','/connections'));
  await new Promise(resolve => setTimeout(resolve,10000));
  const second = selectConnections(await api('GET','/connections'));
  const prior = new Map(first.map(c => [c.connection_id_hash,c]));
  const persistent = second.filter(c => prior.has(c.connection_id_hash));
  const listeners = Array.isArray(configs.listeners) ? configs.listeners.map(l => ({
    type:l.type, port:l.port, listen:l.listen,
    fixed_proxy:!!l.proxy, proxy_alias:l.proxy ? 'policy-'+anon(l.proxy) : null,
    rule_specific:!!l.rule
  })) : null;
  process.stdout.write(JSON.stringify({schema:'dm-gta-r2-route-observation/1',
    observed_at_utc:new Date().toISOString(), mode:configs.mode,
    tun_enabled:configs.tun?.enable, process_detection:configs['find-process-mode'],
    ports:{mixed:configs['mixed-port'],http:configs.port,socks:configs['socks-port']},
    listeners, first, second, observation_interval_seconds:10,
    same_connection_upload_delta_bytes:persistent.reduce((n,c)=>n+Math.max(0,c.upload_bytes-prior.get(c.connection_id_hash).upload_bytes),0),
    same_connection_download_delta_bytes:persistent.reduce((n,c)=>n+Math.max(0,c.download_bytes-prior.get(c.connection_id_hash).download_bytes),0),
    delta_limit:'Only persistent matching R2/rclone connections; closed/new connections excluded. Not total WAN bandwidth.',
    no_config_mutation:true,no_remote_upload:true,no_r2_credentials_accessed:true,
    other_connections_printed:false},null,2)+'\n');
}
main().catch(() => { process.stderr.write('Read-only local controller query failed; no credentials or response body logged.\n'); process.exitCode=1; });
