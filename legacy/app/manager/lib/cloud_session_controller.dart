import 'package:flutter/foundation.dart';

import 'cloud_client.dart';
import 'cloud_models.dart';
import 'cloud_store.dart';

// Owns the single nebula_cloud login shared by the "Cloud" tab (view/connect
// to accessible devices) and the "Host this Mac" tab (self-register this Mac
// as a VDA under the same account) — see the user's "一台设备既可以作为cwa又
// 可以作为vda,所以两者的app界面使用一个就行了" design: one login, two
// capabilities, not two separate sign-ins.
class CloudSessionController extends ChangeNotifier {
  final CloudClient client = CloudClient();
  final CloudSessionStore _store = CloudSessionStore();

  CloudSession? session;
  bool loadingSession = true;

  CloudSessionController() {
    _restore();
  }

  Future<void> _restore() async {
    final saved = await _store.load();
    if (saved == null) {
      loadingSession = false;
      notifyListeners();
      return;
    }
    try {
      session = await client.refresh(saved['baseUrl']!, saved['refreshToken']!);
      await _store.save(session!.baseUrl, session!.refreshToken);
    } catch (_) {
      // Refresh token expired/revoked — fall back to the login form silently.
      await _store.clear();
    }
    loadingSession = false;
    notifyListeners();
  }

  Future<void> login(String baseUrl, String email, String password) async {
    final s = await client.login(baseUrl, email, password);
    session = s;
    await _store.save(s.baseUrl, s.refreshToken);
    notifyListeners();
  }

  Future<void> register(String baseUrl, String email, String password, String displayName) async {
    final s = await client.register(baseUrl, email, password, displayName);
    session = s;
    await _store.save(s.baseUrl, s.refreshToken);
    notifyListeners();
  }

  Future<void> logout() async {
    session = null;
    await _store.clear();
    notifyListeners();
  }

  Future<void> refreshCredit() async {
    if (session == null) return;
    await client.refreshCredit(session!);
    notifyListeners();
  }
}
