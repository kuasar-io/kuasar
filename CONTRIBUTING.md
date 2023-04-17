# Welcome
Welcome to kuasar!

### Main components

The table below lists the core parts of the project:

| Component              | Description                                                                              |
|------------------------|------------------------------------------------------------------------------------------|
| [vmm](vmm)             | Source code of vmm sandbox, including vmm-sandboxer, vmm-task and some building scripts. |
| [quark](quark)         | Quark sandboxer that can run a container by quark.                                       |
| [wasm](wasm)           | Wasm sandboxer that can run a container by WebAssembly runtime.                          |
| [shim](shim)           | An optional workaround of inactive with containerd                                       |
| [documentations](docs) | Documentations of all sandboxer architecture.                                            |
| [tests](tests)         | Benchmark tests and e2e tests directory.                                                 |
| [examples](examples)   | Examples of how to run a container via Kuasar sandboxers.                                |

# Before you get started

## Code of Conduct

Please make sure to read and observe our [Code of Conduct](/CODE_OF_CONDUCT.md).

## Community Expectations

Kuasar is a community project driven by its community which strives to promote a healthy, friendly and productive environment.
The goal of the community is to develop a multi-sandbox ecosystem to meet the requirements under cloud native all-scenario. To build a platform at such scale requires the support of a community with similar aspirations.

- See [Community Membership](https://kuasar.io/docs/community/membership/) for a list of various community roles. With gradual contributions, one can move up in the chain.


# Getting started

- Fork the repository on GitHub
- Read the [setup](https://kuasar.io/docs/developer/build/).


# Your First Contribution

We will help you to contribute in different areas like filing issues, developing features, fixing critical bugs and getting your work reviewed and merged.

If you have questions about the development process, feel free to jump into our [Slack Channel](https://app.slack.com/client/T08PSQ7BQ/C052JRURD8V) or join our [mailing list](https://groups.google.com/forum/#!forum/kuasar).

## Find something to work on

We are always in need of help, be it fixing documentation, reporting bugs or writing some code.
Look at places where you feel best coding practices aren't followed, code refactoring is needed or tests are missing.
Here is how you get started.

### Find a good first topic

There are [multiple repositories](https://github.com/kuasar-io/) within the Kuasar organization.
Each repository has beginner-friendly issues that provide a good first issue.
For example, [kuasar-io/kuasar](https://github.com/kuasar-io/kuasar) has [help wanted](https://github.com/kuasar-io/kuasar/labels/help%20wanted).

Another good way to contribute is to find a documentation improvement, such as a missing/broken link. Please see [Contributing](#contributing) below for the workflow.

#### Work on an issue

When you are willing to take on an issue, you can assign it to yourself. Just reply with `/assign` or `/assign @yourself` on an issue,
then the robot will assign the issue to you and your name will present at `Assignees` list.

### File an Issue

While we encourage everyone to contribute code, it is also appreciated when someone reports an issue.
Issues should be filed under the appropriate Kuasar sub-repository.

*Example:* a Kuasar issue should be opened to [Kuasar/Kuasar](https://github.com/kuasar-io/kuasar/issues).

Please follow the prompted submission guidelines while opening an issue.

# Contributor Workflow

Please do not ever hesitate to ask a question or send a pull request.

This is a rough outline of what a contributor's workflow looks like:

- Create a topic branch from where to base the contribution. This is usually master.
- Make commits of logical units.
- Make sure commit messages are in the proper format (see below).
- Push changes in a topic branch to a personal fork of the repository.
- Submit a pull request to [Kuasar/Kuasar](https://github.com/kuasar-io/kuasar).
- The PR must receive an approval from two maintainers.

## Creating Pull Requests

Pull requests are often called simply "PR".
Kuasar generally follows the standard [GitHub pull request](https://help.github.com/articles/about-pull-requests/) process.
To submit a proposed change, please develop the code/fix and add new test cases.

## Code Review

To make it easier for your PR to receive reviews, consider the reviewers will need you to:

* follow [good coding guidelines](https://github.com/golang/go/wiki/CodeReviewComments).
* write [good commit messages](https://chris.beams.io/posts/git-commit/).
* break large changes into a logical series of smaller patches which individually make easily understandable changes, and in aggregate solve a broader issue.
* label PRs with appropriate reviewers: to do this read the messages the bot sends you to guide you through the PR process.

### Format of the commit message

We follow a rough convention for commit messages that is designed to answer two questions: what changed and why.
The subject line should feature the what and the body of the commit should describe the why.

```
scripts: add test codes for metamanager

this add some unit test codes to improve code coverage for metamanager

Fixes #12
```

The format can be described more formally as follows:

```
<subsystem>: <what changed>
<BLANK LINE>
<why this change was made>
<BLANK LINE>
<footer>
```

The first line is the subject and should be no longer than 70 characters, the second line is always blank, and other lines should be wrapped at 80 characters. This allows the message to be easier to read on GitHub as well as in various git tools.

Note: if your pull request isn't getting enough attention, you can use the reach out on Slack to get help finding reviewers.

## Testing

TO be add

# Before you get started

## Code of Conduct

Please make sure to read and observe our [Code of Conduct](/CODE_OF_CONDUCT.md).

## Community Expectations

Kuasar is a community project driven by its community which strives to promote a healthy, friendly and productive environment.
The goal of the community is to develop a multi-sandbox ecosystem to meet the requirements under cloud native all-scenario. To build a platform at such scale requires the support of a community with similar aspirations.

- See [Community Membership](https://kuasar.io/docs/community/membership/) for a list of various community roles. With gradual contributions, one can move up in the chain.


# Getting started

- Fork the repository on GitHub
- Read the [setup](https://kuasar.io/docs/developer/build/).


# Your First Contribution

We will help you to contribute in different areas like filing issues, developing features, fixing critical bugs and getting your work reviewed and merged.

If you have questions about the development process, feel free to jump into our [Slack Channel](https://app.slack.com/client/T08PSQ7BQ/C052JRURD8V) or join our [mailing list](https://groups.google.com/forum/#!forum/kuasar).

## Find something to work on

We are always in need of help, be it fixing documentation, reporting bugs or writing some code.
Look at places where you feel best coding practices aren't followed, code refactoring is needed or tests are missing.
Here is how you get started.

### Find a good first topic

There are [multiple repositories](https://github.com/kuasar-io/) within the Kuasar organization.
Each repository has beginner-friendly issues that provide a good first issue.
For example, [kuasar-io/kuasar](https://github.com/kuasar-io/kuasar) has [help wanted](https://github.com/kuasar-io/kuasar/labels/help%20wanted).

Another good way to contribute is to find a documentation improvement, such as a missing/broken link. Please see [Contributing](#contributing) below for the workflow.

#### Work on an issue

When you are willing to take on an issue, you can assign it to yourself. Just reply with `/assign` or `/assign @yourself` on an issue,
then the robot will assign the issue to you and your name will present at `Assignees` list.

### File an Issue

While we encourage everyone to contribute code, it is also appreciated when someone reports an issue.
Issues should be filed under the appropriate Kuasar sub-repository.

*Example:* a Kuasar issue should be opened to [Kuasar/Kuasar](https://github.com/kuasar-io/kuasar/issues).

Please follow the prompted submission guidelines while opening an issue.

# Contributor Workflow

Please do not ever hesitate to ask a question or send a pull request.

This is a rough outline of what a contributor's workflow looks like:

- Create a topic branch from where to base the contribution. This is usually master.
- Make commits of logical units.
- Make sure commit messages are in the proper format (see below).
- Push changes in a topic branch to a personal fork of the repository.
- Submit a pull request to [Kuasar/Kuasar](https://github.com/kuasar-io/kuasar).
- The PR must receive an approval from two maintainers.

## Creating Pull Requests

Pull requests are often called simply "PR".
Kuasar generally follows the standard [GitHub pull request](https://help.github.com/articles/about-pull-requests/) process.
To submit a proposed change, please develop the code/fix and add new test cases.

## Code Review

To make it easier for your PR to receive reviews, consider the reviewers will need you to:

* follow [good coding guidelines](https://github.com/golang/go/wiki/CodeReviewComments).
* write [good commit messages](https://chris.beams.io/posts/git-commit/).
* break large changes into a logical series of smaller patches which individually make easily understandable changes, and in aggregate solve a broader issue.
* label PRs with appropriate reviewers: to do this read the messages the bot sends you to guide you through the PR process.

### Format of the commit message

We follow a rough convention for commit messages that is designed to answer two questions: what changed and why.
The subject line should feature the what and the body of the commit should describe the why.

```
scripts: add test codes for metamanager

this add some unit test codes to improve code coverage for metamanager

Fixes #12
```

The format can be described more formally as follows:

```
<subsystem>: <what changed>
<BLANK LINE>
<why this change was made>
<BLANK LINE>
<footer>
```

The first line is the subject and should be no longer than 70 characters, the second line is always blank, and other lines should be wrapped at 80 characters. This allows the message to be easier to read on GitHub as well as in various git tools.

Note: if your pull request isn't getting enough attention, you can use the reach out on Slack to get help finding reviewers.

## Testing

TO be add
