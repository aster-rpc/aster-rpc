using Apache.Fory;
[ForyStruct] public sealed class Payload { public int Count {get;set;} public string Message {get;set;} = ""; }
public static class ProbeHook {
 public static readonly ManualResetEventSlim Published = new(false), Resume = new(false);
 public static int Armed;
 public static void AfterPublish() { if (Interlocked.Exchange(ref Armed, 0)==1) {Published.Set(); if(!Resume.Wait(TimeSpan.FromSeconds(10))) throw new Exception("hook timeout");} }
}
public static class Program {
 public static int Main() {
 using var codec=Fory.Builder().Compatible(true).TrackRef(false).BuildThreadSafe(); codec.Register<Payload>("test","Payload");
 var bytes=codec.Serialize(new Payload{Count=37,Message="correct"});
 ProbeHook.Armed=1;
 var first=Task.Factory.StartNew(()=>codec.Deserialize<Payload>(bytes), CancellationToken.None,TaskCreationOptions.LongRunning,TaskScheduler.Default);
 if(!ProbeHook.Published.Wait(TimeSpan.FromSeconds(10))) throw new Exception("publication not reached");
 try { var second=codec.Deserialize<Payload>(bytes); Console.WriteLine($"second Count={second.Count} Message={second.Message}"); return second.Count==37&&second.Message=="correct"?0:2; }
 finally {ProbeHook.Resume.Set();var result=first.GetAwaiter().GetResult();Console.WriteLine($"first Count={result.Count} Message={result.Message}");}
 }
}