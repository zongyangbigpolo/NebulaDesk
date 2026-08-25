import 'dart:convert';
import 'dart:io';

// Persists just enough to silently resume a Cloud session on next launch:
// the server URL and the long-lived refresh token (never the short-lived
// access token, and never the password). Mirrors VdaStore's file-based
// approach (store.dart) rather than pulling in a secure-keychain dependency —
// same trade-off already accepted for VdaEntry.secret/token in that file.
class CloudSessionStore {
  Future<File> _file() async {
    final home = Platform.environment['HOME'] ?? '.';
    final dir = Directory('$home/Library/Application Support/Nebula');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    return File('${dir.path}/cloud_session.json');
  }

  Future<Map<String, String>?> load() async {
    try {
      final f = await _file();
      if (!f.existsSync()) return null;
      final j = jsonDecode(await f.readAsString()) as Map<String, dynamic>;
      final baseUrl = j['baseUrl'] as String?;
      final refreshToken = j['refreshToken'] as String?;
      if (baseUrl == null || refreshToken == null) return null;
      return {'baseUrl': baseUrl, 'refreshToken': refreshToken};
    } catch (_) {
      return null;
    }
  }

  Future<void> save(String baseUrl, String refreshToken) async {
    final f = await _file();
    await f.writeAsString(jsonEncode({'baseUrl': baseUrl, 'refreshToken': refreshToken}));
  }

  Future<void> clear() async {
    final f = await _file();
    if (f.existsSync()) await f.delete();
  }
}
