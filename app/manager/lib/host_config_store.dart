import 'dart:convert';
import 'dart:io';

import 'vda_launcher.dart';

// Persists this Mac's VDA registration material (see HostConfig) across app
// launches, so "Host this Mac" doesn't need to be re-registered every time —
// mirrors VdaStore/CloudSessionStore's file-based approach (store.dart,
// cloud_store.dart).
class HostConfigStore {
  Future<File> _file() async {
    final home = Platform.environment['HOME'] ?? '.';
    final dir = Directory('$home/Library/Application Support/Nebula');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    return File('${dir.path}/host_config.json');
  }

  Future<HostConfig?> load() async {
    try {
      final f = await _file();
      if (!f.existsSync()) return null;
      return HostConfig.fromJson(jsonDecode(await f.readAsString()) as Map<String, dynamic>);
    } catch (_) {
      return null;
    }
  }

  Future<void> save(HostConfig config) async {
    final f = await _file();
    await f.writeAsString(jsonEncode(config.toJson()));
  }

  Future<void> clear() async {
    final f = await _file();
    if (f.existsSync()) await f.delete();
  }
}
