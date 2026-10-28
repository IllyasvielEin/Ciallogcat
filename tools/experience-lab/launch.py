#!/usr/bin/env python3
"""Launch one native experience session in private X11/network namespaces.

Uses the prepared local runtime. stdin/stdout are the JSON
protocol of native_session.py. It never connects to the user's display.
"""
import argparse
import os
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--run-dir', type=Path, required=True)
parser.add_argument('--lab-root', type=Path, default=ROOT/'target/experience-2026-10-23')
parser.add_argument('--adb', type=Path, default=Path(__file__).with_name('scenario_adb.py'))
parser.add_argument('--scale', type=float, default=1.0)
parser.add_argument('--width', type=int, default=900)
parser.add_argument('--height', type=int, default=560)
args = parser.parse_args()
run = args.run_dir.resolve()
run.mkdir(parents=True, exist_ok=True)
(run/'x11').mkdir(exist_ok=True)
lab = args.lab_root.resolve()
runtime = lab/'runtime'
bwrap = shutil.which('bwrap')
if not bwrap:
    parser.error('bubblewrap is required for independent native desktops')
for required in [runtime/'usr/bin/Xvfb', runtime/'merged-bin', runtime/'host-bin', lab/'bin/ciallogcat']:
    if not required.exists():
        parser.error('prepare the lab runtime first; missing '+str(required))
env = dict(os.environ)
env['LD_LIBRARY_PATH'] = str(runtime/'usr/lib/x86_64-linux-gnu')
env['PYTHONDONTWRITEBYTECODE'] = '1'
command = [bwrap, '--die-with-parent', '--bind', '/', '/', '--dev', '/dev',
           '--ro-bind', '/usr/bin', str(runtime/'host-bin'),
           '--ro-bind', str(runtime/'merged-bin'), '/usr/bin',
           '--bind', str(run/'x11'), '/tmp/.X11-unix', '--unshare-net', '--',
           '/usr/bin/python3', str(Path(__file__).with_name('native_session.py')),
           '--run-dir', str(run), '--binary', str(lab/'bin/ciallogcat'),
           '--adb', str(args.adb.resolve()), '--xvfb', str(runtime/'usr/bin/Xvfb'),
           '--scale', str(args.scale), '--width', str(args.width), '--height', str(args.height)]
os.execve(bwrap, command, env)
