// Data model for the Flutter manager's "Cloud" tab: talks to an optional
// nebula_cloud instance (see server/nebula_cloud) so a user can log in and
// see/connect to every VDA their account can access (owned, granted, via a
// Group, or all of them if they're an admin) without manually configuring
// relay host/port/device-id/token/psk for each one — see
// server/nebula_cloud/README.md's "PSK distribution" section for why the
// connect response can include the actual NEBULA_PSK value.

// A logged-in cloud session: the server we're talking to plus the token
// pair. accessToken is short-lived (minutes) and refreshed transparently by
// CloudClient using refreshToken (which is long-lived and persisted).
class CloudSession {
  final String baseUrl;
  String accessToken;
  String refreshToken;
  DateTime accessExpiresAt;
  String userId;
  String email;
  String displayName;
  String role; // "USER" | "ADMIN"
  // Remaining "connect" credit in seconds (see README.md's "Trial credit"
  // section) — null if the server response didn't include it (older/auth
  // endpoints that don't do the fresh-DB lookup).
  int? creditSeconds;

  CloudSession({
    required this.baseUrl,
    required this.accessToken,
    required this.refreshToken,
    required this.accessExpiresAt,
    required this.userId,
    required this.email,
    required this.displayName,
    required this.role,
    this.creditSeconds,
  });

  bool get accessTokenExpiringSoon =>
      DateTime.now().isAfter(accessExpiresAt.subtract(const Duration(seconds: 30)));
}

// One row in GET /devices — mirrors DeviceListItem in
// server/nebula_cloud/src/services/device-service.ts.
class CloudDevice {
  final String id;
  final String name;
  final String role; // "OWNER" | "ADMIN" | "VIEWER" | "CONTROLLER"
  final String relayDeviceId;
  final String? groupId;
  final DateTime createdAt;
  final DateTime? lastSeenAt;
  final bool online;

  CloudDevice({
    required this.id,
    required this.name,
    required this.role,
    required this.relayDeviceId,
    required this.groupId,
    required this.createdAt,
    required this.lastSeenAt,
    required this.online,
  });

  factory CloudDevice.fromJson(Map<String, dynamic> j) => CloudDevice(
        id: j['id'] as String,
        name: j['name'] as String,
        role: j['role'] as String,
        relayDeviceId: j['relayDeviceId'] as String,
        groupId: j['groupId'] as String?,
        createdAt: DateTime.parse(j['createdAt'] as String),
        lastSeenAt: j['lastSeenAt'] != null ? DateTime.parse(j['lastSeenAt'] as String) : null,
        online: j['online'] as bool,
      );
}

// Response of POST /devices/:id/connect — everything nebula_session needs.
class CloudConnectTicket {
  final String relayHost;
  final int relayPort;
  final String relayDeviceId;
  final String sessionToken;
  final String psk;
  final String role;
  final DateTime expiresAt;
  final int? creditSecondsRemaining;

  CloudConnectTicket({
    required this.relayHost,
    required this.relayPort,
    required this.relayDeviceId,
    required this.sessionToken,
    required this.psk,
    required this.role,
    required this.expiresAt,
    required this.creditSecondsRemaining,
  });

  factory CloudConnectTicket.fromJson(Map<String, dynamic> j) => CloudConnectTicket(
        relayHost: j['relayHost'] as String,
        relayPort: j['relayPort'] as int,
        relayDeviceId: j['relayDeviceId'] as String,
        sessionToken: j['sessionToken'] as String,
        psk: j['psk'] as String,
        role: j['role'] as String,
        expiresAt: DateTime.parse(j['expiresAt'] as String),
        creditSecondsRemaining: j['creditSecondsRemaining'] as int?,
      );
}

// Response of POST /devices/self-register — everything needed to persist a
// HostConfig (vda_launcher.dart) and start hosting immediately.
class CloudSelfRegisterResult {
  final String deviceId;
  final String deviceName;
  final String relayHost;
  final int relayPort;
  final String relayDeviceId;
  final String relayToken;
  final String psk;

  CloudSelfRegisterResult({
    required this.deviceId,
    required this.deviceName,
    required this.relayHost,
    required this.relayPort,
    required this.relayDeviceId,
    required this.relayToken,
    required this.psk,
  });

  factory CloudSelfRegisterResult.fromJson(Map<String, dynamic> j) {
    final device = j['device'] as Map<String, dynamic>;
    final registration = j['registration'] as Map<String, dynamic>;
    return CloudSelfRegisterResult(
      deviceId: device['id'] as String,
      deviceName: device['name'] as String,
      relayHost: registration['relayHost'] as String,
      relayPort: registration['relayPort'] as int,
      relayDeviceId: registration['relayDeviceId'] as String,
      relayToken: registration['relayToken'] as String,
      psk: registration['psk'] as String,
    );
  }
}

// Thrown for any non-2xx response; carries the server's error/message body
// (see nebula_cloud's AppError error handler) so the UI can show it directly.
class CloudApiException implements Exception {
  final int statusCode;
  final String message;
  CloudApiException(this.statusCode, this.message);
  @override
  String toString() => message;
}
