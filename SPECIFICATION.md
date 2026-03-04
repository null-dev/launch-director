# Program Behavioral Specification

The idea here is to create a Rust egui based GUI that can launch locally-developed programs without needing to manually package/install each of those programs.

The program will be launched like so:
```
launch-director --project /path/to/my-project
```

`/path/to/project` will contain have a `mise.toml` with a bunch of [mise tasks](https://mise.jdx.dev/tasks/).

At least two mise tasks will be defined:
- `_launch_director_build`
- `_launch_director_run`

Launch Director will first run the build task to build the project. Then, if it succeeds, it will run the run task to launch the built program.

You can discover the tasks defined by running: `mise tasks ls --local --json`. Launch Director should not directly read the `mise.toml`.

### Running the build task
Launch Director should create a window with an embedded terminal where the build output will be streamed. The bottom of the window should display "Building <dirname of project>..." with a spinner on the left.

If the build fails (it returns a non-zero exit code). The text at the bottom of the window should turn red and the spinner on the left should be replaced with a red cross.

### Running the run task
If the build succeeds, Launch Director should close the build output window and launch the program.

If the program exits with non-zero exit code within 2s after being launched, Launch Director should open a window with an embedded terminal displaying the stderr+stdout of the program. 
The bottom of the window should display "<dirname of project> exited with code <exit code>" in red text with a red cross on the left.

Otherwise, Launch Director's job is done and it should do nothing!