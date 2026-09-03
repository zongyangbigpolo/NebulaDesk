import 'dart:convert';

import 'package:http/http.dart' as http;

import 'cloud_models.dart';

// Thin HTTP wrapper around a nebula_cloud instance's REST API (see
// server/nebula_cloud/README.md). Handles login/register, transparent access
// -token refresh, listing accessible devices, and issuing a connect ticket.
class CloudClient {
  Map<String, String> _headers(CloudSession? session) => {
        'Content-Type': 'application/json',
        if (session != null) 'Authorization': 'Bearer ${session.accessToken}',
      };

  Uri _url(String baseUrl, String path) => Uri.parse('${baseUrl.replaceAll(RegExp(r'/+$'), '')}$path');

  Never _throwForResponse(http.Response resp) {
    String message = 'HTTP ${resp.statusCode}';
    try {
      final body = jsonDecode(resp.body) as Map<String, dynamic>;
      message = (body['message'] ?? body['error'] ?? message).toString();
    } catch (_) {
      // Non-JSON error body (e.g. a proxy's HTML error page) — fall back to the raw status.
    }
    throw CloudApiException(resp.statusCode, message);
  }

  CloudSession _sessionFromAuthResponse(String baseUrl, Map<String, dynamic> body) {
    final user = body['user'] as Map<String, dynamic>;
    return CloudSession(
      baseUrl: baseUrl,
      accessToken: body['accessToken'] as String,
      refreshToken: body['refreshToken'] as String,
      accessExpiresAt: DateTime.now().add(Duration(seconds: body['expiresIn'] as int)),
      userId: user['userId'] as String,
      email: user['email'] as String,
      displayName: user['displayName'] as String,
      role: (user['role'] ?? 'USER') as String,
      creditSeconds: user['creditSeconds'] as int?,
    );
  }

  Future<CloudSession> login(String baseUrl, String email, String password) async {
    final resp = await http.post(
      _url(baseUrl, '/auth/login'),
      headers: _headers(null),
      body: jsonEncode({'email': email, 'password': password}),
    );
    if (resp.statusCode != 200) _throwForResponse(resp);
    return _sessionFromAuthResponse(baseUrl, jsonDecode(resp.body) as Map<String, dynamic>);
  }

  Future<CloudSession> register(String baseUrl, String email, String password, String displayName) async {
    final registerResp = await http.post(
      _url(baseUrl, '/auth/register'),
      headers: _headers(null),
      body: jsonEncode({'email': email, 'password': password, 'displayName': displayName}),
    );
    if (registerResp.statusCode != 201) _throwForResponse(registerResp);
    // Registration doesn't itself return tokens (see app.ts) — log in right after.
    return login(baseUrl, email, password);
  }

  Future<CloudSession> refresh(String baseUrl, String refreshToken) async {
    final resp = await http.post(
      _url(baseUrl, '/auth/refresh'),
      headers: _headers(null),
      body: jsonEncode({'refreshToken': refreshToken}),
    );
    if (resp.statusCode != 200) _throwForResponse(resp);
    return _sessionFromAuthResponse(baseUrl, jsonDecode(resp.body) as Map<String, dynamic>);
  }

  // Ensures `session.accessToken` is still valid, refreshing it in place
  // (mutating `session`) if it's expired/about to expire. Every other method
  // below calls this first so callers never have to think about token TTLs.
  Future<void> _ensureFreshAccessToken(CloudSession session) async {
    if (!session.accessTokenExpiringSoon) return;
    final refreshed = await refresh(session.baseUrl, session.refreshToken);
    session.accessToken = refreshed.accessToken;
    session.refreshToken = refreshed.refreshToken;
    session.accessExpiresAt = refreshed.accessExpiresAt;
  }

  // Refreshes just `session.creditSeconds` from the server (see GET
  // /auth/me) — used after connecting/topping-up to reflect the latest
  // balance without a full token refresh.
  Future<void> refreshCredit(CloudSession session) async {
    await _ensureFreshAccessToken(session);
    final resp = await http.get(_url(session.baseUrl, '/auth/me'), headers: _headers(session));
    if (resp.statusCode != 200) _throwForResponse(resp);
    final user = (jsonDecode(resp.body) as Map<String, dynamic>)['user'] as Map<String, dynamic>;
    session.creditSeconds = user['creditSeconds'] as int?;
  }

  Future<List<CloudDevice>> listDevices(CloudSession session) async {
    await _ensureFreshAccessToken(session);
    final resp = await http.get(_url(session.baseUrl, '/devices'), headers: _headers(session));
    if (resp.statusCode != 200) _throwForResponse(resp);
    final body = jsonDecode(resp.body) as Map<String, dynamic>;
    return (body['devices'] as List)
        .map((e) => CloudDevice.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  Future<CloudConnectTicket> connect(CloudSession session, String deviceId) async {
    await _ensureFreshAccessToken(session);
    final resp = await http.post(
      _url(session.baseUrl, '/devices/$deviceId/connect'),
      headers: _headers(session),
    );
    if (resp.statusCode != 200) _throwForResponse(resp);
    final ticket = CloudConnectTicket.fromJson(jsonDecode(resp.body) as Map<String, dynamic>);
    session.creditSeconds = ticket.creditSecondsRemaining ?? session.creditSeconds;
    return ticket;
  }

  // Registers a device under the caller's own account using the shared
  // enrollment secret — see server/nebula_cloud/README.md's
  // "Self-registration & trial credit". No admin/claim-code step needed
  // first; this is what the "Host this Mac" tab calls.
  Future<CloudSelfRegisterResult> selfRegisterDevice(CloudSession session, String name, String enrollmentToken) async {
    await _ensureFreshAccessToken(session);
    final resp = await http.post(
      _url(session.baseUrl, '/devices/self-register'),
      headers: _headers(session),
      body: jsonEncode({'name': name, 'enrollmentToken': enrollmentToken}),
    );
    if (resp.statusCode != 201) _throwForResponse(resp);
    return CloudSelfRegisterResult.fromJson(jsonDecode(resp.body) as Map<String, dynamic>);
  }
}
