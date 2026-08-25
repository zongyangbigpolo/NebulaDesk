import 'dart:convert';
import 'dart:io';

import 'models.dart';

// Launches and tracks a single supervised nebula_vda process — this Mac's
// "Host this Mac" duty (see cloud_tab.dart / host_tab.dart), spawned exactly
// the way the Flutter manager already spawns nebula_session in
// session_launcher.dart, using the same NEBULA_STATUS: stdout protocol (see
// core/inc/SessionStatus.h) and the same "secrets via environment, never
// argv" convention (app/vda/main.mm now honors NEBULA_PSK/NEBULA_RELAY_TOKEN
// env vars for exactly this reason).
//
// Unlike SessionLauncher (which can run several independent CWA sessions
// concurrently), a Mac only ever hosts as ONE VDA at a time, so this tracks
// at most a single process.
class VdaLauncher {
  Process? _proc;

  int? get pid => _proc?.pid;
  bool get isRunning => _proc != null;

  String _vdaExecutablePath() {
    // 1) Inside a built .app: Contents/MacOS/<manager> + Helpers/nebula_vda
    final exeDir = File(Platform.resolvedExecutable).parent;
    final bundled = File('${exeDir.path}/../Helpers/nebula_vda');
    if (bundled.existsSync()) return bundled.resolveSymbolicLinksSync();

    // 2) Dev fallback: repo build output.
    const dev = 'build/app/vda/nebula_vda';
    if (File(dev).existsSync()) return File(dev).absolute.path;

    // 3) Env override for testing.
    final env = Platform.environment['NEBULA_VDA_BIN'];
    if (env != null && File(env).existsSync()) return env;

    return bundled.path; // best effort
  }

  Future<Process> start(
    HostConfig config, {
    required void Function(SessionState) onState,
    required void Function(int exitCode) onExit,
  }) async {
    if (_proc != null) {
      throw StateError('Already hosting — stop the current session first');
    }
    final exe = _vdaExecutablePath();
    final args = <String>[
      '--port', '7000',
      '--relay', config.relayHost,
      '--relay-port', '${config.relayPort}',
      '--device', config.relayDeviceId,
    ];
    final proc = await Process.start(
      exe,
      args,
      environment: {
        'NEBULA_PSK': config.psk,
        'NEBULA_RELAY_TOKEN': config.relayToken,
      },
    );
    _proc = proc;

    proc.stdout.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      const prefix = 'NEBULA_STATUS:';
      if (line.startsWith(prefix)) {
        onState(parseSessionState(line.substring(prefix.length).trim()));
      }
    });
    proc.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      // ignore: avoid_print
      print('[vda] $line');
    });

    proc.exitCode.then((code) {
      _proc = null;
      onExit(code);
    });
    return proc;
  }

  void stop() {
    _proc?.kill(ProcessSignal.sigterm);
  }
}

// Long-lived VDA registration material, persisted locally by HostConfigStore
// after a successful POST /devices/self-register (or /device-claims/redeem).
// Mirrors what nebula_cloud's `registration` response returns — see
// server/nebula_cloud/README.md's "Self-registration & trial credit".
class HostConfig {
  final String deviceName;
  final String relayHost;
  final int relayPort;
  final String relayDeviceId;
  final String relayToken;
  final String psk;

  HostConfig({
    required this.deviceName,
    required this.relayHost,
    required this.relayPort,
    required this.relayDeviceId,
    required this.relayToken,
    required this.psk,
  });

  Map<String, dynamic> toJson() => {
        'deviceName': deviceName,
        'relayHost': relayHost,
        'relayPort': relayPort,
        'relayDeviceId': relayDeviceId,
        'relayToken': relayToken,
        'psk': psk,
      };

  factory HostConfig.fromJson(Map<String, dynamic> j) => HostConfig(
        deviceName: j['deviceName'] as String,
        relayHost: j['relayHost'] as String,
        relayPort: j['relayPort'] as int,
        relayDeviceId: j['relayDeviceId'] as String,
        relayToken: j['relayToken'] as String,
        psk: j['psk'] as String,
      );
}
