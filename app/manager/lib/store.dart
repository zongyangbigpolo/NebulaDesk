import 'dart:convert';
import 'dart:io';

import 'models.dart';

// Persists the VDA list to a JSON file in the user's Application Support dir.
class VdaStore {
  Future<File> _file() async {
    final home = Platform.environment['HOME'] ?? '.';
    final dir = Directory('$home/Library/Application Support/Nebula');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    return File('${dir.path}/vdas.json');
  }

  Future<List<VdaEntry>> load() async {
    try {
      final f = await _file();
      if (!f.existsSync()) return [];
      final list = jsonDecode(await f.readAsString()) as List;
      return list.map((e) => VdaEntry.fromJson(e as Map<String, dynamic>)).toList();
    } catch (_) {
      return [];
    }
  }

  Future<void> save(List<VdaEntry> vdas) async {
    final f = await _file();
    await f.writeAsString(jsonEncode(vdas.map((e) => e.toJson()).toList()));
  }
}
