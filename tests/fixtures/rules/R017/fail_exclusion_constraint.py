# R017: AddConstraint with ExclusionConstraint - should fail
from django.db import migrations
from django.contrib.postgres.constraints import ExclusionConstraint


class Migration(migrations.Migration):
    dependencies = []

    operations = [
        migrations.AddConstraint(
            model_name='booking',
            constraint=ExclusionConstraint(
                name='exclude_overlapping',
                expressions=[('daterange', '&&')],
            ),
        ),
    ]
