import ctypes,csv,time,struct,datetime,subprocess
from pathlib import Path
base=Path('/tmp/typers-markdown-qa-20261025')
lib=ctypes.CDLL('/usr/lib/libproc.dylib')
class T(ctypes.Structure): _fields_=[('numer',ctypes.c_uint32),('denom',ctypes.c_uint32)]
t=T();ctypes.CDLL('/usr/lib/libSystem.B.dylib').mach_timebase_info(ctypes.byref(t));scale=t.numer/t.denom/1e9
last=None
with (base/'resources2.csv').open('w',buffering=1) as f:
 w=csv.writer(f);w.writerow(['time_utc','elapsed_s','pid','phase','cpu_percent','resident_mib','physical_footprint_mib','cpu_total_s'])
 start=time.monotonic()
 while not (base/'stop').exists():
  try: pid=int(subprocess.check_output(['pgrep','-f','^/Applications/Typers.app/Contents/MacOS/velotype$'],text=True).strip())
  except: time.sleep(.5); continue
  buf=ctypes.create_string_buffer(4096)
  rc=lib.proc_pid_rusage(pid,2,ctypes.byref(buf))
  if rc: time.sleep(.5); continue
  now=time.monotonic();u,s=struct.unpack_from('QQ',buf.raw,16);rss,phys=struct.unpack_from('QQ',buf.raw,64)
  total=(u+s)*scale
  cpu=0 if last is None or last[2]!=pid else 100*(total-last[1])/(now-last[0])
  phase=(base/'phase.txt').read_text().strip()
  w.writerow([datetime.datetime.now(datetime.timezone.utc).isoformat(),round(now-start,3),pid,phase,round(cpu,3),round(rss/1048576,3),round(phys/1048576,3),round(total,6)])
  last=(now,total,pid)
  time.sleep(.5)
print('monitor finished')
