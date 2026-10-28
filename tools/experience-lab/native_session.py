#!/usr/bin/env python3
"""Operate/capture only an isolated GUI child owned by this script."""
import ctypes as C
import ctypes.util
import json
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import argparse
import select
import signal

parser=argparse.ArgumentParser(description="Isolated native GUI experience session; JSON commands on stdin")
parser.add_argument("--run-dir",type=Path,required=True)
parser.add_argument("--binary",type=Path,required=True)
parser.add_argument("--adb",type=Path,required=True)
parser.add_argument("--xvfb",type=Path,required=True)
parser.add_argument("--scale",type=float,default=1.0)
parser.add_argument("--width",type=int,default=900)
parser.add_argument("--height",type=int,default=560)
args=parser.parse_args()

ROOT=args.run_dir.resolve()
ROOT.mkdir(parents=True,exist_ok=True)
ROOT.joinpath('lab').mkdir(exist_ok=True)
trace=ROOT.joinpath('actions.jsonl').open('a',encoding='utf-8')
def record(value):
 value['observed_unix_ms']=time.time_ns()//1000000
 trace.write(json.dumps(value,ensure_ascii=False)+'\n');trace.flush()
# One cleanup boundary owns every startup stage, including Xlib loading and a
# failed GUI spawn after Xvfb has already started.
xvfb=child=timer=None
read_fd=write_fd=None

def signal_owned_group(process, signum):
 if process is None:return
 try:os.killpg(process.pid,signum)
 except ProcessLookupError:pass

def stop_owned_group(process):
 if process is None:return
 signal_owned_group(process,signal.SIGTERM)
 try:process.wait(timeout=5)
 except subprocess.TimeoutExpired:
  signal_owned_group(process,signal.SIGKILL);process.wait(timeout=3)
 finally:
  # The leader may have exited before its owned ADB/helper descendants.
  signal_owned_group(process,signal.SIGKILL)

def diagnostic_tail(path):
 with path.open('rb') as stream:
  stream.seek(0,os.SEEK_END);length=stream.tell()
  stream.seek(max(0,length-16384));data=stream.read(16384)
 return ('[earlier diagnostic bytes omitted]\n' if length>16384 else '')+data.decode('utf-8',errors='replace')

def expire():
 for process in (child,xvfb):signal_owned_group(process,signal.SIGKILL)
 try:
  for process in (child,xvfb):
   if process is not None:process.wait(timeout=2)
 finally:os._exit(124)

def interrupted(signum,_frame):raise SystemExit(128+signum)
signal.signal(signal.SIGTERM,interrupted)
signal.signal(signal.SIGINT,interrupted)

try:
 timer=threading.Timer(1800,expire);timer.daemon=True;timer.start()
 read_fd,write_fd=os.pipe()
 xvfb=subprocess.Popen([str(args.xvfb.resolve()),'-displayfd',str(write_fd),'-screen','0','2000x1400x24','-nolisten','tcp','-noreset','-ac','-pn'],pass_fds=(write_fd,),stdout=subprocess.DEVNULL,stderr=ROOT.joinpath('xvfb.stderr').open('w'),start_new_session=True)
 record({'event':'xvfb_started','xvfb_pid':xvfb.pid})
 os.close(write_fd);write_fd=None
 if not select.select([read_fd],[],[],15)[0]:
  raise RuntimeError('Xvfb did not provide a display in 15 seconds')
 display_number=os.read(read_fd,32).decode().strip();os.close(read_fd);read_fd=None
 if not display_number.isdigit():
  raise RuntimeError('Xvfb failed: '+diagnostic_tail(ROOT/'xvfb.stderr'))
 DISPLAY=':'+display_number
 os.environ['DISPLAY']=DISPLAY
 os.environ['GDK_BACKEND']='x11'
 os.environ.pop('WAYLAND_DISPLAY',None)
 os.environ.pop('WAYLAND_SOCKET',None)

 X=C.CDLL(ctypes.util.find_library('X11'))
 T=C.CDLL(ctypes.util.find_library('Xtst'))
 P=C.c_void_p; U=C.c_ulong; I=C.c_int
 def sig(lib,name,args,result):
  f=getattr(lib,name);f.argtypes=args;f.restype=result;return f
 sig(X,'XOpenDisplay',[C.c_char_p],P)
 sig(X,'XDefaultRootWindow',[P],U)
 sig(X,'XGetSelectionOwner',[P,U],U)
 sig(X,'XInternAtom',[P,C.c_char_p,I],U)
 sig(X,'XGetWindowProperty',[P,U,U,C.c_long,C.c_long,I,U,C.POINTER(U),C.POINTER(I),C.POINTER(U),C.POINTER(U),C.POINTER(P)],I)
 sig(X,'XQueryTree',[P,U,C.POINTER(U),C.POINTER(U),C.POINTER(C.POINTER(U)),C.POINTER(C.c_uint)],I)
 sig(X,'XFree',[P],I)
 sig(X,'XMoveResizeWindow',[P,U,I,I,C.c_uint,C.c_uint],I)
 sig(X,'XRaiseWindow',[P,U],I)
 sig(X,'XSetInputFocus',[P,U,I,U],I)
 sig(X,'XGetInputFocus',[P,C.POINTER(U),C.POINTER(I)],I)
 sig(X,'XSync',[P,I],I)
 sig(X,'XStringToKeysym',[C.c_char_p],U)
 sig(X,'XKeysymToKeycode',[P,U],C.c_uint)
 sig(X,'XGetGeometry',[P,U,C.POINTER(U),C.POINTER(I),C.POINTER(I),C.POINTER(C.c_uint),C.POINTER(C.c_uint),C.POINTER(C.c_uint),C.POINTER(C.c_uint)],I)
 sig(X,'XTranslateCoordinates',[P,U,U,I,I,C.POINTER(I),C.POINTER(I),C.POINTER(U)],I)
 sig(T,'XTestFakeKeyEvent',[P,C.c_uint,I,U],I)
 sig(T,'XTestFakeButtonEvent',[P,C.c_uint,I,U],I)
 sig(T,'XTestFakeMotionEvent',[P,I,I,I,U],I)

 display=X.XOpenDisplay(DISPLAY.encode())
 if not display:raise RuntimeError('X display unavailable')
 root=X.XDefaultRootWindow(display)
 pid_atom=X.XInternAtom(display,b'_NET_WM_PID',0)
 env=dict(os.environ,DISPLAY=DISPLAY,WINIT_UNIX_BACKEND='x11',GDK_BACKEND='x11',LIBGL_ALWAYS_SOFTWARE='1',
  CIALLOGCAT_CONFIG_DIR=str(ROOT/'config'),CIALLOGCAT_ADB=str(args.adb.resolve()),CIALLOGCAT_LAB_DIR=str(ROOT/'lab'),
  WINIT_X11_SCALE_FACTOR=str(args.scale))
 child=subprocess.Popen([str(args.binary.resolve())],env=env,
  stdin=subprocess.DEVNULL,stdout=(ROOT/'app.stdout').open('w'),stderr=(ROOT/'app.stderr').open('w'),start_new_session=True)
 record({'event':'started','pid':child.pid,'xvfb_pid':xvfb.pid,'display':DISPLAY,'binary':str(args.binary.resolve()),'binary_sha256':hashlib.sha256(args.binary.read_bytes()).hexdigest()})
 window=None
 def pid(window):
  typ=U();fmt=I();count=U();left=U();data=P()
  if X.XGetWindowProperty(display,window,pid_atom,0,1,0,0,C.byref(typ),C.byref(fmt),C.byref(count),C.byref(left),C.byref(data))!=0:return None
  value=C.cast(data,C.POINTER(U))[0] if data and count.value else None
  if data:X.XFree(data)
  return value
 def children(window):
  r=U();p=U();array=C.POINTER(U)();n=C.c_uint()
  if not X.XQueryTree(display,window,C.byref(r),C.byref(p),C.byref(array),C.byref(n)):return []
  values=[array[i] for i in range(n.value)]
  if array:X.XFree(array)
  return values
 def focus():
  if child.poll() is not None:raise RuntimeError('owned child has exited')
  if pid(window)!=child.pid:raise RuntimeError('window no longer belongs to owned child')
  current=U();revert=I()
  for _ in range(8):
   X.XRaiseWindow(display,window);X.XSetInputFocus(display,window,2,0);X.XSync(display,0);time.sleep(.1)
   X.XGetInputFocus(display,C.byref(current),C.byref(revert))
   if current.value==window:return
  raise RuntimeError(f'refusing input: owned window {window} not focused; actual {current.value}')
 def keycode(name):
  symbol=X.XStringToKeysym(name.encode('ascii'));code=X.XKeysymToKeycode(display,symbol)
  if not code:raise RuntimeError('key has no keycode: '+name)
  return code
 def stroke(key,mods=()):
  focus();codes=[keycode(name) for name in mods]
  for code in codes:T.XTestFakeKeyEvent(display,code,1,0)
  X.XSync(display,0);time.sleep(.06 if codes else .005)
  code=keycode(key);T.XTestFakeKeyEvent(display,code,1,0);X.XSync(display,0);time.sleep(.015);T.XTestFakeKeyEvent(display,code,0,0)
  X.XSync(display,0);time.sleep(.03 if codes else .005)
  for code in reversed(codes):T.XTestFakeKeyEvent(display,code,0,0)
  X.XSync(display,0);time.sleep(.03)
 def text(value):
  names={' ':'space','/':'slash','.':'period','-':'minus','_':'underscore',':':'colon','\\':'backslash'}
  for char in value:
   if not char.isascii():raise RuntimeError('ASCII-only injected text')
   mods=['Shift_L'] if char.isupper() or char in '_:' else []
   stroke(names.get(char,char.lower()),mods)
 def geometry():
  r=U();x=I();y=I();w=C.c_uint();h=C.c_uint();border=C.c_uint();depth=C.c_uint()
  X.XGetGeometry(display,window,C.byref(r),C.byref(x),C.byref(y),C.byref(w),C.byref(h),C.byref(border),C.byref(depth))
  return w.value,h.value
 def screenshot(name):
  focus();time.sleep(.25)
  import gi
  gi.require_version('Gdk','3.0');gi.require_version('GdkX11','3.0')
  from gi.repository import Gdk,GdkX11
  gd=Gdk.Display.open(DISPLAY)
  own=GdkX11.X11Window.foreign_new_for_display(gd,window)
  w,h=geometry();pixbuf=Gdk.pixbuf_get_from_window(own,0,0,w,h)
  if not pixbuf:raise RuntimeError('owned window image unavailable')
  path=ROOT/(name+'.png');pixbuf.savev(str(path),'png',[],[])
  return str(path)
 def clipboard_owned(name):
  # Read only after XRes proves selection owner belongs to our own GUI child.
  R=C.CDLL(ctypes.util.find_library('XRes'))
  class Spec(C.Structure):_fields_=[('client',U),('mask',C.c_uint)]
  class Value(C.Structure):_fields_=[('spec',Spec),('length',C.c_long),('value',P)]
  sig(R,'XResQueryClientIds',[P,C.c_long,C.POINTER(Spec),C.POINTER(C.c_long),C.POINTER(C.POINTER(Value))],I)
  sig(R,'XResGetClientPid',[C.POINTER(Value)],I)
  sig(R,'XResClientIdsDestroy',[C.c_long,C.POINTER(Value)],None)
  atom=X.XInternAtom(display,b'CLIPBOARD',0)
  owner=X.XGetSelectionOwner(display,atom)
  spec=Spec(owner,2);n=C.c_long();values=C.POINTER(Value)()
  R.XResQueryClientIds(display,1,C.byref(spec),C.byref(n),C.byref(values))
  pids=[R.XResGetClientPid(C.byref(values[i])) for i in range(n.value)]
  if values:R.XResClientIdsDestroy(n,values)
  if not any(value == child.pid or Path(f'/proc/{child.pid}/task/{value}').exists() for value in pids):
   return {'clipboard_unverified':True,'owner':owner,'owner_pid':pids,'expected_pid':child.pid}
  import gi
  gi.require_version('Gtk','3.0');gi.require_version('Gdk','3.0')
  from gi.repository import Gtk,Gdk,GLib
  result=[]
  clip=Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
  clip.request_text(lambda _clipboard,value,_data:result.append(value),None)
  deadline=time.monotonic()+5
  context=GLib.MainContext.default()
  while not result and time.monotonic()<deadline:
   while context.pending():context.iteration(False)
   time.sleep(.005)
  if not result:raise RuntimeError('owned clipboard request timed out')
  if X.XGetSelectionOwner(display,atom)!=owner:raise RuntimeError('clipboard ownership changed during own-data read')
  value=result[0]
  if value is None:raise RuntimeError('owned clipboard has no text')
  data=value.encode('utf-8');path=ROOT/(name+'.txt');path.write_bytes(data)
  return {'clipboard_bytes':len(data),'clipboard_sha256':hashlib.sha256(data).hexdigest(),'path':str(path),'owner_pid':child.pid}

 def click(x,y,button=1):
  focus();rx=I();ry=I();sub=U()
  X.XTranslateCoordinates(display,window,root,int(x),int(y),C.byref(rx),C.byref(ry),C.byref(sub))
  T.XTestFakeMotionEvent(display,-1,rx.value,ry.value,0)
  T.XTestFakeButtonEvent(display,button,1,0);T.XTestFakeButtonEvent(display,button,0,0);X.XSync(display,0)
  time.sleep(.15)
 deadline=time.monotonic()+30
 while time.monotonic()<deadline and window is None:
  for candidate in children(root):
   if pid(candidate)==child.pid:window=candidate;break
   for nested in children(candidate):
    if pid(nested)==child.pid:window=nested;break
   if window:break
  if child.poll() is not None:raise RuntimeError(diagnostic_tail(ROOT/'app.stderr'))
  time.sleep(.1)
 if window is None:raise RuntimeError('owned window was not mapped')
 scale=args.scale
 X.XMoveResizeWindow(display,window,80,80,round(args.width*scale),round(args.height*scale));X.XSync(display,0);time.sleep(1)
 print(json.dumps({'ready':True,'pid':child.pid,'display':DISPLAY,'window':window,'size':geometry(),'screenshot':screenshot('initial')}),flush=True)
 for line in sys.stdin:
  command=json.loads(line)
  if not isinstance(command,dict) or 'op' not in command:
   print(json.dumps({'error':'each command requires an op, e.g. {"op":"shot","name":"state"}'}),flush=True);continue
  record({'event':'command','command':command})
  if command['op']=='quit':break
  if command['op']=='key':stroke(command['key'],command.get('mods',[]))
  elif command['op']=='text':text(command['text'])
  elif command['op']=='paste':
   import gi
   gi.require_version('Gtk','3.0');gi.require_version('Gdk','3.0')
   from gi.repository import Gtk,Gdk,GLib
   clip=Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD);clip.set_text(command['text'],-1)
   while GLib.MainContext.default().pending():GLib.MainContext.default().iteration(False)
   stroke('v',['Control_L'])
   deadline=time.monotonic()+.4
   while time.monotonic()<deadline:
    while GLib.MainContext.default().pending():GLib.MainContext.default().iteration(False)
    time.sleep(.005)
  elif command['op']=='scroll':
   for _ in range(min(200,abs(int(command.get('steps',1))))):click(command.get('x',450),command.get('y',260),5 if command.get('steps',1)>0 else 4)
  elif command['op']=='hover':
   focus();rx=I();ry=I();sub=U()
   X.XTranslateCoordinates(display,window,root,int(command['x']),int(command['y']),C.byref(rx),C.byref(ry),C.byref(sub))
   T.XTestFakeMotionEvent(display,-1,rx.value,ry.value,0);X.XSync(display,0)
  elif command['op']=='click':click(command['x'],command['y'],command.get('button',1))
  elif command['op']=='resize':X.XMoveResizeWindow(display,window,80,80,command['width'],command['height']);X.XSync(display,0)
  elif command['op']=='clipboard':
   result=clipboard_owned(command.get('name','clipboard'));record({'event':'clipboard','result':result});print(json.dumps(result),flush=True)
   continue
  elif command['op']!='shot':raise RuntimeError('unknown operation')
  time.sleep(command.get('wait',.25))
  result={'ok':True,'size':geometry(),'screenshot':screenshot(command['name']) if command.get('name') else None};record({'event':'result','result':result});print(json.dumps(result),flush=True)
finally:
 if timer is not None:timer.cancel()
 for descriptor in (read_fd,write_fd):
  if descriptor is not None:
   try:os.close(descriptor)
   except OSError:pass
 try:
  try:stop_owned_group(child)
  finally:stop_owned_group(xvfb)
 finally:
  try:record({'event':'stopped','child_exit':None if child is None else child.returncode,
              'xvfb_exit':None if xvfb is None else xvfb.returncode})
  finally:trace.close()
