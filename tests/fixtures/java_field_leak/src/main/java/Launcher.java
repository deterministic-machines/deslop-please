import com.example.runqueue.app.App;

/** In the default package, which shares no package with the others. */
public class Launcher {
    public static void main(String[] args) {
        System.out.println(App.run(new int[] {1, 2, 3}));
    }
}
