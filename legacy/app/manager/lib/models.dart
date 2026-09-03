// VDA connection record persisted by the manager.
class VdaEntry {
  String name;
  String host;
  int port;
  String relayHost; // optional; empty = direct connection
  int relayPort;
  String deviceId;
  String token;
  String secret; // shared secret (PSK); passed to session via env

  VdaEntry({
    required this.name,
    required this.host,
    required this.port,
    this.relayHost = '',
    this.relayPort = 7100,
    this.deviceId = '',
    this.token = '',
    this.secret = 'nebula-default-psk',
  });

  Map<String, dynamic> toJson() => {
        'name': name,
        'host': host,
        'port': port,
        'relayHost': relayHost,
        'relayPort': relayPort,
        'deviceId': deviceId,
        'token': token,
        'secret': secret,
      };

  factory VdaEntry.fromJson(Map<String, dynamic> j) => VdaEntry(
        name: j['name'] as String,
        host: j['host'] as String,
        port: j['port'] as int,
        relayHost: (j['relayHost'] ?? j['relayUrl'] ?? '') as String,
        relayPort: (j['relayPort'] ?? 7100) as int,
        deviceId: (j['deviceId'] ?? '') as String,
        token: (j['token'] ?? '') as String,
        secret: (j['secret'] ?? 'nebula-default-psk') as String,
      );
}

enum SessionState { idle, connecting, connected, streaming, direct, error, disconnected }

SessionState parseSessionState(String s) {
  switch (s) {
    case 'connecting':
      return SessionState.connecting;
    case 'connected':
      return SessionState.connected;
    case 'streaming':
      return SessionState.streaming;
    case 'direct':
      return SessionState.direct;
    case 'error':
      return SessionState.error;
    case 'disconnected':
      return SessionState.disconnected;
  }
  return SessionState.idle;
}
