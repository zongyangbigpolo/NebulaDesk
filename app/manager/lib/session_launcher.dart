import 'dart:convert';
import 'dart:io';

import 'cloud_models.dart';
import 'models.dart';

// Launches and tracks an independent nebula_session process per VDA connection.
//
// Contract (see core/inc/SessionStatus.h):
//   * non-sensitive args via argv (--host/--port/--title)
//   * secret via environment NEBULA_PSK (never argv -> not visible in `ps`)
//   * session writes "NEBULA_STATUS:<state>" lines to stdout; we parse them here
class SessionLauncher {
  final Map<int, Process> _procs = {};

  // Resolves the path to the bundled session helper next to the manager app.
  String _sessionExecutablePath() {
    // 1) Inside a built .app: Contents/MacOS/<manager> + Helpers/nebula_session
    final exeDir = File(Platform.resolvedExecutable).parent; // .../Contents/MacOS
    final bundled = File('${exeDir.path}/../Helpers/nebula_session');
    if (bundled.existsSync()) return bundled.resolveSymbolicLinksSync();

    // 2) Dev fallback: repo build output.
    const dev = 'build/app/session/nebula_session';
    if (File(dev).existsSync()) return File(dev).absolute.path;

    // 3) Env override for testing.
    final env = Platform.environment['NEBULA_SESSION_BIN'];
    if (env != null && File(env).existsSync()) return env;

    return bundled.path; // best effort
  }

  Future<Process> launch(
    VdaEntry vda, {
    required void Function(SessionState) onState,
    required void Function(int exitCode) onExit,
  }) {
    final args = <String>[
      '--host', vda.host,
      '--port', '${vda.port}',
      '--title', vda.name,
      if (vda.relayHost.isNotEmpty) ...[
        '--relay', vda.relayHost,
        '--relay-port', '${vda.relayPort}',
        '--device', vda.deviceId,
      ],
    ];
    return _spawn(
      args,
      environment: {
        'NEBULA_PSK': vda.secret,
        'NEBULA_RELAY_TOKEN': vda.token,
      },
      onState: onState,
      onExit: onExit,
    );
  }

  // Launches a session using a nebula_cloud connect ticket (see
  // cloud_client.dart's CloudClient.connect) instead of a manually-configured
  // VdaEntry: relay routing, the short-lived session JWT, and the PSK all
  // come from the ticket, so there's nothing left for the user to type in.
  // --host/--port are required argv but are ignored once --relay is present
  // (see CwaClient::connect in core/src/CwaClient.mm), so any placeholder
  // works.
  Future<Process> launchCloud(
    CloudConnectTicket ticket, {
    required String title,
    required void Function(SessionState) onState,
    required void Function(int exitCode) onExit,
  }) {
    final args = <String>[
      '--host', '127.0.0.1',
      '--port', '0',
      '--title', title,
      '--relay', ticket.relayHost,
      '--relay-port', '${ticket.relayPort}',
      '--device', ticket.relayDeviceId,
    ];
    return _spawn(
      args,
      environment: {
        'NEBULA_PSK': ticket.psk,
        'NEBULA_RELAY_TOKEN': ticket.sessionToken,
      },
      onState: onState,
      onExit: onExit,
    );
  }

  Future<Process> _spawn(
    List<String> args, {
    required Map<String, String> environment,
    required void Function(SessionState) onState,
    required void Function(int exitCode) onExit,
  }) async {
    final exe = _sessionExecutablePath();
    final proc = await Process.start(exe, args, environment: environment);

    proc.stdout.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      const prefix = 'NEBULA_STATUS:';
      if (line.startsWith(prefix)) {
        onState(parseSessionState(line.substring(prefix.length).trim()));
      }
    });
    // Surface session stderr (human logs) to the manager console for debugging.
    proc.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      // ignore: avoid_print
      print('[session] $line');
    });

    _procs[proc.pid] = proc;
    proc.exitCode.then((code) {
      _procs.remove(proc.pid);
      onExit(code);
    });
    return proc;
  }

  void terminate(int pid) {
    _procs[pid]?.kill(ProcessSignal.sigterm);
  }

  void terminateAll() {
    for (final p in _procs.values) {
      p.kill(ProcessSignal.sigterm);
    }
    _procs.clear();
  }
}
